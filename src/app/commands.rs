use super::*;

pub(crate) fn submit_input(app: &mut App) -> Result<()> {
    let input = app.input.as_str().trim().to_owned();
    if input.is_empty() {
        return Ok(());
    }
    app.input.push_history();
    if let Some(command) = input
        .strip_prefix('!')
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        app.input.clear();
        return request_shell_approval(app, command.to_owned());
    }
    if input.starts_with('/') {
        if let Some(command) = commands::parse(&input) {
            app.input.clear();
            return execute_command(app, command);
        }
        if let Some(prompt) = expand_custom_command(app, &input) {
            app.input.set(prompt);
            return submit_input(app);
        }
        app.input.clear();
        app.current.push_entry(DisplayEntry {
            kind: DisplayKind::Error,
            content: DisplayContent::Markdown(format!("未知命令，请使用 /help 查看命令：{input}")),
        });
        return Ok(());
    }
    let Some(runner) = app.current.runner.clone() else {
        app.current.status = "请打开提供商设置配置 API Key".into();
        return Ok(());
    };
    // A concurrent submit must never overwrite the running `active_task`;
    // reject it instead (the service layer mirrors this check for non-active
    // target sessions).
    if app.current.busy || app.current.active_task.is_some() {
        return Err(anyhow::anyhow!("session is busy with another request"));
    }
    app.input.clear();
    app.current.push_entry(DisplayEntry {
        kind: DisplayKind::User,
        content: DisplayContent::Markdown(input.clone()),
    });
    app.current.conversation.push(ConversationItem::Message {
        role: Role::User,
        content: input.clone(),
    });
    app.storage
        .append_message(&app.current.session_id, Role::User, &input)?;
    for (label, content) in collect_file_context(app, &input) {
        app.current.conversation.push(ConversationItem::Context {
            label: label.clone(),
            content: content.clone(),
        });
        app.storage
            .append_context(&app.current.session_id, &label, &content)?;
        app.current.push_entry(DisplayEntry {
            kind: DisplayKind::System,
            content: DisplayContent::Markdown(format!("已附加文件 @{label}")),
        });
    }
    refresh_sessions(app)?;
    // No fixed item/byte trim here: context capacity is the model window minus
    // the output reservation and system overhead, and overflow is handled by
    // full-turn compaction (falling back to a hinted hard-limit trim). The
    // anchored meter below feeds the authoritative context budget: real usage
    // covers the prefix, the calibrated estimate covers the increment.
    app.current.context_used_tokens = estimate_used_tokens(
        app.current.usage_anchor.as_ref(),
        &app.current.conversation,
        app.current.token_calibration,
    );
    app.current.busy = true;
    app.current.agent_phase = AgentPhase::Thinking;
    app.current.model_phase = ModelPhase::Idle;
    app.current.status = "准备请求中…… | Esc 取消".into();
    let items = app.current.conversation.clone();
    let events = app.current.agent_tx.clone();
    // Stamp the session's current anchor onto this run's runner snapshot so
    // in-run compaction thresholds compare the anchored estimate.
    let runner = runner.with_usage_anchor(app.current.usage_anchor.clone());
    app.current.active_task = Some(tokio::spawn(async move {
        runner.run(items, events).await;
    }));
    app.current.trim_entries();
    Ok(())
}

fn expand_custom_command(app: &App, input: &str) -> Option<String> {
    let mut parts = input[1..].trim().splitn(2, char::is_whitespace);
    let name = parts.next()?;
    let arguments = parts.next().unwrap_or("").trim();
    let command = app
        .config
        .commands
        .iter()
        .find(|command| command.name == name)?;
    if command.template.trim().is_empty() {
        return None;
    }
    Some(
        command
            .template
            .replace("{args}", arguments)
            .replace("{workspace}", &app.workspace.display().to_string()),
    )
}

fn collect_file_context(app: &App, input: &str) -> Vec<(String, String)> {
    let mut contexts = Vec::new();
    let mut total = 0usize;
    for token in input
        .split_whitespace()
        .filter_map(|token| token.strip_prefix('@'))
    {
        let path = token.trim_matches(|character: char| {
            matches!(character, ',' | '.' | ':' | ';' | ')' | ']' | '}')
        });
        if path.is_empty() || contexts.iter().any(|(label, _)| label == path) {
            continue;
        }
        let Ok(resolved) = app.registry.workspace().resolve_existing(path) else {
            continue;
        };
        let Ok(metadata) = std::fs::metadata(&resolved) else {
            continue;
        };
        if !metadata.is_file() || metadata.len() > 64 * 1024 {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&resolved) else {
            continue;
        };
        let remaining = (256 * 1024usize).saturating_sub(total);
        if remaining == 0 {
            break;
        }
        let mut content = content;
        if content.len() > remaining {
            content.truncate(remaining);
            while !content.is_char_boundary(content.len()) {
                content.pop();
            }
            content.push_str("\n[context truncated]");
        }
        total = total.saturating_add(content.len());
        contexts.push((path.to_owned(), content));
    }
    contexts
}

fn memory_command(app: &mut App, argument: Option<&str>) -> Result<()> {
    let workspace = app.workspace.to_string_lossy().into_owned();
    let argument = argument.unwrap_or("list").trim();
    let mut parts = argument.splitn(2, char::is_whitespace);
    let action = parts.next().unwrap_or("list").to_ascii_lowercase();
    let rest = parts.next().map(str::trim).unwrap_or_default();
    let source_session = app.current.session_id.clone();
    let source_turn = app.storage.head_turn_id(&source_session)?;
    let mut render = |records: Vec<MemoryRecord>| {
        let mut content = String::from("## 工作区记忆\n");
        if records.is_empty() {
            content.push_str("\n暂无记忆或候选。\n");
        }
        for record in records {
            let status = match record.status.as_str() {
                "candidate" => "候选（待确认）",
                status if record.recallable => status,
                _ => "待核实（来源已失效）",
            };
            content.push_str(&format!(
                "\n- #{} [{}] {}\n  {}\n",
                record.id, status, record.title, record.content
            ));
            if record.source_session_id.is_some() || record.evidence.is_some() {
                content.push_str(&format!(
                    "  来源：{}{}\n",
                    record
                        .source_session_id
                        .as_deref()
                        .map(|id| id.chars().take(8).collect::<String>())
                        .unwrap_or_else(|| "手动".into()),
                    record
                        .evidence
                        .as_deref()
                        .map(|evidence| format!(" · {evidence}"))
                        .unwrap_or_default()
                ));
            }
            if content.len() >= 64 * 1024 {
                content.truncate(64 * 1024);
                content.push_str("\n[记忆列表已截断]\n");
                break;
            }
        }
        app.current.push_entry(DisplayEntry {
            kind: DisplayKind::System,
            content: DisplayContent::Markdown(content),
        });
        app.current.status = "记忆管理".into();
    };
    match action.as_str() {
        "list" => render(app.storage.list_memories(&workspace, None, false)?),
        "search" => render(app.storage.list_memories(&workspace, Some(rest), false)?),
        "add" | "candidate" => {
            let mut values = rest.splitn(2, '|');
            let title = values.next().unwrap_or_default().trim();
            let content = values.next().unwrap_or_default().trim();
            let record = app.storage.create_memory(
                &workspace,
                if action == "candidate" {
                    "candidate"
                } else {
                    "explicit"
                },
                title,
                content,
                None,
                if action == "candidate" {
                    "candidate"
                } else {
                    "active"
                },
                Some(&source_session),
                source_turn.as_deref(),
                None,
                Some("user-managed"),
                app.config.memory.max_entries,
                app.config.memory.max_candidates,
                app.config.memory.max_entry_bytes,
                app.config.memory.max_total_bytes,
            )?;
            app.current.push_entry(DisplayEntry {
                kind: DisplayKind::System,
                content: DisplayContent::Markdown(format!(
                    "已{}记忆 #{}：**{}**",
                    if action == "candidate" {
                        "加入候选"
                    } else {
                        "保存"
                    },
                    record.id,
                    record.title
                )),
            });
        }
        "confirm" => {
            let id: i64 = rest.parse().map_err(|_| anyhow::anyhow!("记忆编号无效"))?;
            let record = app.storage.confirm_memory(&workspace, id)?;
            app.current.status = format!("已确认记忆 #{}", record.id);
        }
        "delete" | "remove" => {
            let id: i64 = rest.parse().map_err(|_| anyhow::anyhow!("记忆编号无效"))?;
            app.storage.delete_memory(&workspace, id)?;
            app.current.status = format!("已删除记忆 #{id}");
        }
        "edit" => {
            let mut values = rest.splitn(2, char::is_whitespace);
            let id: i64 = values
                .next()
                .ok_or_else(|| anyhow::anyhow!("用法：/memory edit <编号> 标题 | 内容"))?
                .parse()
                .map_err(|_| anyhow::anyhow!("记忆编号无效"))?;
            let mut fields = values.next().unwrap_or_default().splitn(2, '|');
            let title = fields.next().unwrap_or_default().trim();
            let content = fields.next().unwrap_or_default().trim();
            app.storage.update_memory(
                &workspace,
                id,
                title,
                content,
                app.config.memory.max_entry_bytes,
            )?;
            app.current.status = format!("已更新记忆 #{id}");
        }
        _ => {
            app.current.status =
                "用法：/memory [list|search 关键词|add 标题 | 内容|candidate 标题 | 内容|confirm 编号|edit 编号 标题 | 内容|delete 编号]".into();
        }
    }
    Ok(())
}

pub(crate) fn execute_command(app: &mut App, command: Command) -> Result<()> {
    match command {
        Command::Help => {
            app.current.push_entry(DisplayEntry {
                kind: DisplayKind::System,
                content: DisplayContent::Markdown(
                    "## 命令\n\n`/new` `/rename` `/fork` `/delete`\n`/undo` `/redo` `/compact` `/export [路径]` `/todo [add|doing|done|undo|edit|remove|clear]` `/memory [search|add|candidate|confirm|edit|delete]` `/diff`\n`/plan` `/build` `/explore` `/model` `/provider`\n\nCtrl+P 或 Ctrl+X 打开命令面板 | @ 文件 | ! Shell"
                        .into(),
                ),
            });
            app.current.status = "命令帮助".into();
        }
        Command::NewSession => create_session(app)?,
        Command::Provider => {
            // Provider configuration is handled by the WebUI settings screen
            // (POST /api/config/provider); the TUI settings panel is gone.
            app.current.status = format!(
                "当前提供商：{} · {}",
                app.config.provider.preset.label(),
                app.config.provider.model
            );
        }
        Command::Model(model) => {
            if let Some(model) = model {
                if model.trim().is_empty() {
                    app.current.status = "模型不能为空".into();
                } else {
                    apply_model_choice(app, model.trim().to_owned())?;
                }
            } else {
                app.current.status = format!("当前模型：{}", app.config.provider.model);
            }
        }
        Command::Agent(agent) => {
            if let Some(name) = agent {
                if let Some(configured) = app.config.agents.iter().find(|item| item.name == name) {
                    app.current.mode = configured.mode;
                    app.registry.set_mode(app.current.mode);
                    app.storage
                        .set_session_mode(&app.current.session_id, app.current.mode.as_str())?;
                    // Force a fresh provider context so the new mode contract is
                    // sent as the stable system prefix on the next request.
                    app.storage.clear_response_id(&app.current.session_id)?;
                    app.current.status = format!("Agent：{} | 模式：{}", name, app.current.mode);
                    app.current.push_entry(DisplayEntry {
                        kind: DisplayKind::System,
                        content: DisplayContent::Markdown(format!(
                            "Agent 模式已切换为 **{}**。下一次模型请求将使用 {} 执行约束。",
                            app.current.mode.as_str().to_ascii_uppercase(),
                            app.current.mode.as_str()
                        )),
                    });
                } else {
                    app.current.status = format!("未知 Agent：{name}");
                }
            } else {
                app.current.status = format!("当前 Agent 模式：{}", app.current.mode);
            }
        }
        Command::Memory(argument) => memory_command(app, argument.as_deref())?,
        Command::Mode(mode) => {
            switch_mode(app, mode)?;
            app.current.push_entry(DisplayEntry {
                kind: DisplayKind::System,
                content: DisplayContent::Markdown(format!(
                    "Agent 模式已切换为 **{}**。下一次模型请求将使用 {} 执行约束。",
                    mode.as_str().to_ascii_uppercase(),
                    mode.as_str()
                )),
            });
        }
        Command::Clear => {
            app.current.entries.clear();
            app.current.reset_thinking_state();
            app.current.status = "显示已清空，会话历史仍保留".into();
        }
        Command::Quit => app.should_quit = true,
        Command::Rename(title) => {
            let Some(title) = title
                .as_deref()
                .map(str::trim)
                .filter(|title| !title.is_empty())
            else {
                app.input.set("/rename ");
                app.current.status = "请输入新会话名称：/rename <名称>".into();
                return Ok(());
            };
            app.storage.rename_session(&app.current.session_id, title)?;
            refresh_sessions(app)?;
            app.current.status = format!("会话已重命名为 {title}");
        }
        Command::Delete => {
            let deleted = app.current.session_id.clone();
            let deleted_ids = app.storage.delete_session(&deleted)?;
            let next = match app.storage.latest_session(&app.workspace)? {
                Some(session_id) => session_id,
                None => app.storage.create_session(&app.workspace)?,
            };
            activate_session(app, next)?;
            let deleted_ids = deleted_ids.into_iter().collect::<HashSet<_>>();
            for session_id in &deleted_ids {
                if let Some(mut runtime) = app.background.remove(session_id) {
                    runtime.shutdown();
                }
                app.child_status.remove(session_id);
                app.child_batches.remove(session_id);
            }
            app.child_batches.retain(|_, children| {
                children.retain(|child_id| !deleted_ids.contains(child_id));
                !children.is_empty()
            });
            let _ = app.storage.purge_soft_deleted_snapshots();
            refresh_sessions(app)?;
            app.current.status = "会话已删除".into();
        }
        Command::Fork => {
            let fork = app.storage.fork_session(&app.current.session_id)?;
            activate_session(app, fork)?;
            refresh_sessions(app)?;
            app.current.status = "会话已创建分支".into();
        }
        Command::Undo => {
            let detached = app.storage.head_turn_id(&app.current.session_id)?;
            if app.storage.undo(&app.current.session_id)? {
                app.storage.clear_response_id(&app.current.session_id)?;
                let rollback_message = if let Some(turn_id) = detached {
                    restore_snapshots(app, &turn_id, SnapshotDirection::Backward)
                } else {
                    None
                };
                reload_current_session(app)?;
                refresh_sessions(app)?;
                app.current.status = match rollback_message {
                    Some(message) => format!("已撤销上一轮；{message}"),
                    None => "已撤销上一轮".into(),
                };
            } else {
                app.current.status = "没有可撤销的内容".into();
            }
        }
        Command::Redo => {
            if app.storage.redo(&app.current.session_id)? {
                let advanced = app.storage.head_turn_id(&app.current.session_id)?;
                app.storage.clear_response_id(&app.current.session_id)?;
                let rollback_message = if let Some(turn_id) = advanced {
                    restore_snapshots(app, &turn_id, SnapshotDirection::Forward)
                } else {
                    None
                };
                reload_current_session(app)?;
                refresh_sessions(app)?;
                app.current.status = match rollback_message {
                    Some(message) => format!("已重做上一轮；{message}"),
                    None => "已重做上一轮".into(),
                };
            } else {
                app.current.status = "没有可重做的内容".into();
            }
        }
        Command::Todo(action) => handle_todo_command(app, action)?,
        Command::Compact(focus) => {
            let Some(runner) = app.current.runner.clone() else {
                app.current.status = "请打开提供商设置配置 API Key".into();
                return Ok(());
            };
            let mut items = app.current.conversation.clone();
            let events = app.current.agent_tx.clone();
            let focus = focus.map(|value| value.trim().to_owned());
            app.current.busy = true;
            app.current.status = "准备压缩上下文…… | Esc 取消".into();
            app.current.active_task = Some(tokio::spawn(async move {
                match runner
                    .compact_context(&mut items, focus.as_deref(), &events)
                    .await
                {
                    Ok(_) => {
                        let _ = events.send(AgentEvent::Completed { items }).await;
                    }
                    Err(error) => {
                        trim_conversation(&mut items);
                        let _ = events.send(AgentEvent::CompactionFailed(error)).await;
                        let _ = events.send(AgentEvent::Completed { items }).await;
                    }
                }
            }));
        }
        Command::Uncompact => {
            if app
                .storage
                .restore_latest_compaction(&app.current.session_id)?
            {
                // Restoring rewrote the stored history; the anchor's item
                // index no longer describes it reliably.
                app.current.usage_anchor = None;
                let session_id = app.current.session_id.clone();
                activate_session(app, session_id)?;
                app.current.status = "已恢复最近一次压缩".into();
            } else {
                app.current.status = "没有可恢复的压缩检查点".into();
            }
        }
        Command::Export(path) => export_session(app, path)?,
        Command::Diff => start_diff(app)?,
    }
    Ok(())
}

pub(crate) fn handle_todo_command(app: &mut App, action: TodoCommand) -> Result<()> {
    match action {
        TodoCommand::Show => {
            let (done, total) = todo_progress(&app.current.todos);
            let mut content = format!("## 任务清单 {done}/{total}\n");
            for (index, task) in app.current.todos.iter().enumerate() {
                content.push_str(&format!(
                    "- {} {}. {}\n",
                    task.status.symbol(),
                    index + 1,
                    task.title
                ));
            }
            app.current.push_entry(DisplayEntry {
                kind: DisplayKind::System,
                content: DisplayContent::Markdown(content),
            });
            app.current.status = if total == 0 {
                "任务清单为空".into()
            } else {
                format!("任务清单 {done}/{total}")
            };
        }
        TodoCommand::Add(title) => {
            let mut tasks = app.current.todos.clone();
            tasks.push(TodoTask::new(title, TodoStatus::Pending));
            apply_todo_tasks(app, tasks)?;
            app.current.status = "任务已添加".into();
        }
        TodoCommand::Doing(index) => {
            update_todo_status(app, index, TodoStatus::InProgress, "任务已标记为进行中")?;
        }
        TodoCommand::Done(index) => {
            update_todo_status(app, index, TodoStatus::Done, "任务已完成")?;
        }
        TodoCommand::Undo(index) => {
            update_todo_status(app, index, TodoStatus::Pending, "任务已标记为待处理")?;
        }
        TodoCommand::Edit(index, title) => {
            let mut tasks = app.current.todos.clone();
            let Some(task) = tasks.get_mut(index.checked_sub(1).unwrap_or(usize::MAX)) else {
                app.current.status = "任务序号不存在".into();
                return Ok(());
            };
            task.title = title;
            task.updated_at = chrono::Utc::now().to_rfc3339();
            apply_todo_tasks(app, tasks)?;
            app.current.status = "任务已更新".into();
        }
        TodoCommand::Remove(index) => {
            let mut tasks = app.current.todos.clone();
            if index == 0 || index > tasks.len() {
                app.current.status = "任务序号不存在".into();
                return Ok(());
            }
            tasks.remove(index - 1);
            apply_todo_tasks(app, tasks)?;
            app.current.status = "任务已删除".into();
        }
        TodoCommand::Clear => {
            apply_todo_tasks(app, Vec::new())?;
            app.current.status = "任务清单已清空".into();
        }
    }
    Ok(())
}

fn todo_progress(tasks: &[TodoTask]) -> (usize, usize) {
    (
        tasks
            .iter()
            .filter(|task| task.status == TodoStatus::Done)
            .count(),
        tasks.len(),
    )
}

fn update_todo_status(
    app: &mut App,
    index: usize,
    status: TodoStatus,
    message: &str,
) -> Result<()> {
    let mut tasks = app.current.todos.clone();
    let Some(task) = tasks.get_mut(index.checked_sub(1).unwrap_or(usize::MAX)) else {
        app.current.status = "任务序号不存在".into();
        return Ok(());
    };
    task.status = status;
    task.updated_at = chrono::Utc::now().to_rfc3339();
    apply_todo_tasks(app, tasks)?;
    app.current.status = message.into();
    Ok(())
}

fn apply_todo_tasks(app: &mut App, tasks: Vec<TodoTask>) -> Result<()> {
    app.storage.replace_tasks(&app.current.session_id, &tasks)?;
    app.current.set_todos(tasks);
    Ok(())
}

pub(crate) fn export_session(app: &mut App, requested: Option<String>) -> Result<()> {
    let requested = requested
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty());
    let default_filename = format!("1h-agent-{}.md", app.current.session_id);
    let filename = requested.unwrap_or(default_filename.as_str());
    let target = app
        .registry
        .workspace()
        .resolve_new(filename)
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let mut output = String::new();
    if !app.current.todos.is_empty() {
        let (done, total) = todo_progress(&app.current.todos);
        output.push_str(&format!("## 任务清单（{done}/{total}）\n\n"));
        for task in &app.current.todos {
            let checkbox = if task.status == TodoStatus::Done {
                "x"
            } else {
                " "
            };
            let suffix = if task.status == TodoStatus::InProgress {
                "（进行中）"
            } else {
                ""
            };
            output.push_str(&format!("- [{checkbox}] {}{suffix}\n", task.title));
        }
        output.push('\n');
    }
    for item in &app.current.conversation {
        if let ConversationItem::Message { role, content } = item {
            let label = match role {
                Role::System => "System",
                Role::User => "You",
                Role::Assistant => "Agent",
            };
            output.push_str(&format!("## {label}\n\n{content}\n\n"));
        }
        if output.len() > 5 * 1024 * 1024 {
            output.push_str("\n[export truncated]\n");
            break;
        }
    }
    std::fs::write(&target, output)
        .with_context(|| format!("cannot write export {}", target.display()))?;
    let display_path = match target.strip_prefix(&app.workspace) {
        Ok(path) => path.display(),
        Err(_) => target.display(),
    };
    app.current.status = format!("对话已导出到工作区 {}", display_path);
    Ok(())
}
