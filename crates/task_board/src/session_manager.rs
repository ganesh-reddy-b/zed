use anyhow::{Context as _, Result, anyhow};
use agent_ui::{
    AgentPanel, AgentThreadSource, TerminalId, TerminalInitCommand,
    terminal_thread_metadata_store::TerminalThreadMetadata,
};
use chrono::Utc;
use gpui::{AsyncWindowContext, Context, Entity, SharedString, Task, Window};
use settings::Settings as _;
use workspace::{OpenMode, Workspace};

use crate::{
    BoardTask, BoardTaskId, TaskBoardSettings, TaskBoardStore, TaskSession, TaskSessionId,
};

/// Spawn a new agent session for a task: a Terminal Thread running the
/// configured agent CLI inside the task's worktree workspace.
pub fn spawn_session(
    task_id: BoardTaskId,
    agent_key: String,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Task<Result<()>> {
    let store = TaskBoardStore::global(cx);
    let Some(task) = store.read(cx).task(task_id).cloned() else {
        return Task::ready(Err(anyhow!("task no longer exists")));
    };
    // Sessions run in the primary project's worktree; the other worktrees are
    // siblings in the same workspace.
    let Some(worktree_path) = task.worktree_path() else {
        return Task::ready(Err(anyhow!("start the task first to create its worktree")));
    };
    let settings = TaskBoardSettings::get_global(cx);
    let Some(agent) = settings.agents.get(&agent_key).cloned() else {
        return Task::ready(Err(anyhow!(
            "no agent named \"{agent_key}\" is configured in the task_board settings"
        )));
    };

    let custom_title: SharedString = format!("{agent_key}: {}", task.title).into();
    let resolve = resolve_task_workspace(&task, workspace, window, cx);

    cx.spawn_in(window, async move |_, cx| {
        let task_workspace = resolve.await?;
        let panel = agent_panel_for_workspace(&task_workspace, cx).await?;

        let terminal_id = task_workspace.update_in(cx, |workspace, window, cx| {
            let terminal_id = panel.update(cx, |panel, cx| {
                panel.spawn_terminal_with_init_command(
                    Some(worktree_path.clone()),
                    Some(custom_title.clone()),
                    TerminalInitCommand::Override(agent.command.clone()),
                    AgentThreadSource::TaskBoard,
                    window,
                    cx,
                )
            });
            workspace.focus_panel::<AgentPanel>(window, cx);
            terminal_id
        })?;
        let terminal_id =
            terminal_id.context("the task's project does not support terminals")?;

        cx.update(|_, cx| {
            TaskBoardStore::global(cx).update(cx, |store, cx| {
                store.upsert_session(
                    TaskSession {
                        session_id: TaskSessionId::new(),
                        task_id,
                        terminal_id: Some(terminal_id.to_key_string()),
                        agent: agent_key,
                        label: None,
                        working_directory: Some(worktree_path),
                        resume_command: agent.resume_command.clone(),
                        archived: false,
                        created_at: Utc::now(),
                        archived_at: None,
                    },
                    cx,
                );
            });
        })?;
        Ok(())
    })
}

/// Focus a session's terminal if it is live, or restore it (fresh shell in
/// the saved cwd, typing the resume command) if it is dormant or archived.
pub fn open_session(
    session_id: TaskSessionId,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Task<Result<()>> {
    let store = TaskBoardStore::global(cx);
    let Some(session) = store.read(cx).session(session_id).cloned() else {
        return Task::ready(Err(anyhow!("session no longer exists")));
    };
    let Some(task) = store.read(cx).task(session.task_id).cloned() else {
        return Task::ready(Err(anyhow!("the session's task no longer exists")));
    };
    if !task.has_worktree() {
        return Task::ready(Err(anyhow!(
            "the task's worktree was cleaned up; reopen the task first"
        )));
    }

    let agent_command = TaskBoardSettings::get_global(cx)
        .agents
        .get(&session.agent)
        .map(|agent| agent.command.clone());
    let resolve = resolve_task_workspace(&task, workspace, window, cx);

    cx.spawn_in(window, async move |_, cx| {
        let task_workspace = resolve.await?;
        let panel = agent_panel_for_workspace(&task_workspace, cx).await?;

        let live_terminal_id = session
            .terminal_id
            .as_deref()
            .and_then(|key| TerminalId::from_key_string(key).ok())
            .filter(|terminal_id| {
                panel.read_with(cx, |panel, _| panel.has_terminal(*terminal_id))
            });

        if let Some(terminal_id) = live_terminal_id {
            task_workspace.update_in(cx, |workspace, window, cx| {
                panel.update(cx, |panel, cx| {
                    panel.activate_terminal(terminal_id, true, window, cx);
                });
                workspace.focus_panel::<AgentPanel>(window, cx);
            })?;
            return Ok(());
        }

        // The terminal isn't in this window's panel, but the session may
        // still be live in another window — respawning here would start a
        // second agent in the same worktree and orphan the running one.
        let live_elsewhere = cx.update(|_, cx| {
            TaskBoardStore::global(cx).read(cx).session_runtime(session_id)
                != crate::SessionRuntimeState::Dormant
        })?;
        anyhow::ensure!(
            !live_elsewhere,
            "this session is running in another window; switch to it there or \
             close it first"
        );

        // The terminal is gone — respawn a shell in the saved cwd and type
        // the agent's resume command (falling back to its start command).
        let resume_command = session
            .resume_command
            .clone()
            .or(agent_command)
            .context("no resume command configured for this session's agent")?;
        let terminal_id = TerminalId::new();
        let title: SharedString = format!("{}: {}", session.agent, task.title).into();

        task_workspace.update_in(cx, |workspace, window, cx| {
            let worktree_paths = workspace.project().read(cx).worktree_paths(cx);
            let metadata = TerminalThreadMetadata {
                terminal_id,
                title: title.clone(),
                custom_title: session.label.clone(),
                created_at: session.created_at,
                worktree_paths,
                remote_connection: None,
                working_directory: session.working_directory.clone(),
            };
            panel.update(cx, |panel, cx| {
                panel.restore_terminal_with_init_command(
                    metadata,
                    TerminalInitCommand::Override(resume_command),
                    true,
                    AgentThreadSource::TaskBoard,
                    Some(workspace),
                    window,
                    cx,
                );
            });
            workspace.focus_panel::<AgentPanel>(window, cx);
        })?;

        cx.update(|_, cx| {
            TaskBoardStore::global(cx).update(cx, |store, cx| {
                let mut session = session;
                session.terminal_id = Some(terminal_id.to_key_string());
                session.archived = false;
                session.archived_at = None;
                store.upsert_session(session, cx);
            });
        })?;
        Ok(())
    })
}

/// Archive a session: close its terminal (if live anywhere in this window)
/// and keep the durable record for later restore.
pub fn archive_session(session_id: TaskSessionId, window: &mut Window, cx: &mut gpui::App) {
    let store = TaskBoardStore::global(cx);
    let terminal_id = store
        .read(cx)
        .session(session_id)
        .and_then(|session| session.terminal_id.as_deref())
        .and_then(|key| TerminalId::from_key_string(key).ok());

    if let Some(terminal_id) = terminal_id {
        store.update(cx, |store, cx| {
            store.close_live_terminal(terminal_id, window, cx);
        });
    }
    store.update(cx, |store, cx| store.archive_session(session_id, cx));
}

fn resolve_task_workspace(
    task: &BoardTask,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Task<Result<Entity<Workspace>>> {
    if !task.has_worktree() {
        return Task::ready(Err(anyhow!("the task has no worktree")));
    }
    // The session's workspace holds every worktree the task spans so agents
    // can reach all of its repositories.
    let worktree_paths = task.worktree_paths.clone();
    let group_key = (task.extra_project_ids.is_empty())
        .then(|| {
            TaskBoardStore::global(cx)
                .read(cx)
                .project(task.project_id)
                .map(|project| project.group_key())
        })
        .flatten();
    let Some(multi_workspace) = workspace.multi_workspace().cloned() else {
        return Task::ready(Err(anyhow!("task board requires a windowed workspace")));
    };

    cx.spawn_in(window, async move |source_workspace, cx| {
        let multi_workspace = multi_workspace.upgrade().context("window was closed")?;
        multi_workspace
            .update_in(cx, |multi_workspace, window, cx| {
                multi_workspace.find_or_create_workspace(
                    worktree_paths,
                    None,
                    group_key,
                    |_, _, _| Task::ready(Ok(None)),
                    None,
                    OpenMode::Activate,
                    Some(source_workspace.clone()),
                    window,
                    cx,
                )
            })?
            .await
    })
}

async fn agent_panel_for_workspace(
    workspace: &Entity<Workspace>,
    cx: &mut AsyncWindowContext,
) -> Result<Entity<AgentPanel>> {
    let existing = workspace.read_with(cx, |workspace, cx| workspace.panel::<AgentPanel>(cx));
    if let Some(panel) = existing {
        return Ok(panel);
    }

    let panel = AgentPanel::load(workspace.downgrade(), cx.clone()).await?;
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.panel::<AgentPanel>(cx).unwrap_or_else(|| {
            workspace.add_panel(panel.clone(), window, cx);
            panel.clone()
        })
    })
}
