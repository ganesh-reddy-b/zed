use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use askpass::AskPassDelegate;
use git::repository::{PushOptions, UpstreamTracking};
use git_ui_core::{askpass_modal::AskPassModal, worktree_service};
use gpui::{
    AsyncWindowContext, Context, Entity, SharedString, Task, TaskExt as _, WeakEntity, Window,
};
use project::{ProjectGroupKey, git_store::Repository};
use settings::Settings as _;
use util::path_list::PathList;
use workspace::{OpenMode, Workspace};
use zed_actions::{CreateWorktree, NewWorktreeBranchTarget};

use agent_ui::{
    thread_metadata_store::ArchivedGitWorktree,
    thread_worktree_archive::{
        build_root_plan, find_or_create_repository, remove_root, restore_worktree_via_git,
    },
};

use crate::{
    BoardTaskId, SessionRuntimeState, TaskBoardArchivedWorktree, TaskBoardSettings,
    TaskBoardStore, TaskStatus, session_manager,
};

/// Derive a git-friendly slug from a task title.
pub(crate) fn slugify(title: &str) -> String {
    let mut slug = String::new();
    for ch in title.chars() {
        if slug.len() >= 40 {
            break;
        }
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() { "task".to_string() } else { slug }
}

/// Pick a slug that collides with neither an existing branch nor an existing
/// worktree directory (a leftover from an interrupted start would otherwise
/// block the task forever).
fn unique_slug(
    base: &str,
    branch_prefix: &str,
    existing_branches: &HashSet<String>,
    taken_worktree_names: &HashSet<String>,
) -> String {
    let is_free = |candidate: &str| {
        !existing_branches.contains(&format!("{branch_prefix}{candidate}"))
            && !taken_worktree_names.contains(candidate)
    };
    if is_free(base) {
        return base.to_string();
    }
    for suffix in 2.. {
        let candidate = format!("{base}-{suffix}");
        if is_free(&candidate) {
            return candidate;
        }
    }
    unreachable!("suffix loop is unbounded")
}

/// Start a task: create a git worktree + branch for it (opening the task's
/// project in this window first if needed), and move it to In Progress.
pub fn start_task(
    task_id: BoardTaskId,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Task<Result<()>> {
    let store = TaskBoardStore::global(cx);
    let Some(task) = store.read(cx).task(task_id).cloned() else {
        return Task::ready(Err(anyhow!("task no longer exists")));
    };
    if task.has_worktree() {
        return Task::ready(Err(anyhow!("this task already has a worktree")));
    }
    let mut board_projects = Vec::new();
    for project_id in task.all_project_ids() {
        let Some(board_project) = store.read(cx).project(project_id).cloned() else {
            return Task::ready(Err(anyhow!(
                "one of the task's projects is no longer registered"
            )));
        };
        if board_project.remote_connection.is_some() {
            return Task::ready(Err(anyhow!(
                "starting tasks in remote projects is not supported yet"
            )));
        }
        board_projects.push(board_project);
    }
    let single_project = board_projects.len() == 1;

    // One workspace holding every project the task spans, so a single
    // worktree-creation pass covers all their repositories.
    let mut combined_paths: Vec<PathBuf> = Vec::new();
    for board_project in &board_projects {
        for path in board_project.main_worktree_paths.ordered_paths() {
            if !combined_paths.contains(path) {
                combined_paths.push(path.clone());
            }
        }
    }
    let combined_paths = PathList::new(&combined_paths);

    let branch_prefix = TaskBoardSettings::get_global(cx).branch_prefix.clone();
    let group_key = board_projects[0].group_key();
    let source_key = ProjectGroupKey::from_project(workspace.project().read(cx), cx);
    let source_matches = if single_project {
        source_key.matches(&group_key)
    } else {
        source_key.path_list() == &combined_paths
    };
    let multi_workspace = workspace.multi_workspace().cloned();
    let title = task.title;

    cx.spawn_in(window, async move |source_workspace, cx| {
        let project_workspace = if source_matches {
            source_workspace
                .upgrade()
                .context("workspace was closed")?
        } else {
            let multi_workspace = multi_workspace
                .context("task board requires a windowed workspace")?
                .upgrade()
                .context("window was closed")?;
            multi_workspace
                .update_in(cx, |multi_workspace, window, cx| {
                    multi_workspace.find_or_create_workspace(
                        combined_paths.clone(),
                        None,
                        single_project.then(|| group_key.clone()),
                        |_, _, _| Task::ready(Ok(None)),
                        None,
                        OpenMode::Add,
                        Some(source_workspace.clone()),
                        window,
                        cx,
                    )
                })?
                .await?
        };

        let repositories = wait_for_repositories(&project_workspace, cx).await?;
        let repository = repositories
            .first()
            .cloned()
            .context("the task's project has no git repository")?;

        // The branch and worktrees are created in every repository the task
        // spans, so avoid collisions across all of them.
        let mut existing_branches: HashSet<String> = HashSet::new();
        let mut taken_worktree_names: HashSet<String> = HashSet::new();
        for repository in &repositories {
            let branches = repository
                .update(cx, |repository, _| repository.branches())
                .await
                .map_err(|_| anyhow!("branch scan was canceled"))??;
            existing_branches.extend(
                branches
                    .branches
                    .iter()
                    .map(|branch| branch.name().to_string()),
            );

            let worktrees = repository
                .update(cx, |repository, _| repository.worktrees())
                .await
                .map_err(|_| anyhow!("worktree scan was canceled"))??;
            for worktree in worktrees {
                if worktree.is_main {
                    continue;
                }
                // Zed lays linked worktrees out as .../<name>/<repo_dir>, so
                // the parent directory carries the worktree's name.
                if let Some(name) = worktree
                    .path
                    .parent()
                    .and_then(|parent| parent.file_name())
                    .and_then(|name| name.to_str())
                {
                    taken_worktree_names.insert(name.to_string());
                }
            }
        }

        let slug = unique_slug(
            &slugify(&title),
            &branch_prefix,
            &existing_branches,
            &taken_worktree_names,
        );
        let branch_name = format!("{branch_prefix}{slug}");

        let created = project_workspace
            .update_in(cx, |workspace, window, cx| {
                worktree_service::create_worktree_workspace(
                    workspace,
                    &CreateWorktree {
                        worktree_name: Some(slug.clone()),
                        branch_target: NewWorktreeBranchTarget::CurrentBranch,
                    },
                    window,
                    None,
                    cx,
                )
            })?
            .await?;

        let new_paths: Vec<PathBuf> = created.workspace.read_with(cx, |workspace, cx| {
            workspace
                .project()
                .read(cx)
                .visible_worktrees(cx)
                .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
                .collect()
        });
        // The created workspace carries any non-git roots of the source
        // project over unchanged; they must be neither branched (checkout
        // would fail) nor recorded as task worktrees (finishing would try to
        // archive the original folder).
        let new_paths: Vec<PathBuf> = new_paths
            .into_iter()
            .filter(|path| path.join(".git").exists())
            .collect();
        anyhow::ensure!(!new_paths.is_empty(), "no worktree was created");

        for path in &new_paths {
            repository
                .update(cx, |repository, _| {
                    repository.checkout_branch_in_worktree(branch_name.clone(), path.clone(), true)
                })
                .await
                .map_err(|_| anyhow!("branch creation was canceled"))??;
        }

        cx.update(|_, cx| {
            TaskBoardStore::global(cx).update(cx, |store, cx| {
                store.update_task(
                    task_id,
                    |task| {
                        task.branch_name = Some(branch_name.clone());
                        task.worktree_paths = PathList::new(&new_paths);
                    },
                    cx,
                );
                let needs_move = store
                    .task(task_id)
                    .is_some_and(|task| task.status != TaskStatus::InProgress);
                if needs_move {
                    store.move_task(task_id, TaskStatus::InProgress, usize::MAX, cx);
                }
            });
        })?;
        Ok(())
    })
}

/// Publish the task's branch (auto-setting the upstream) and open the git
/// host's pull request creation page, recording the PR URL on the task and
/// moving it to In Review.
pub fn create_pull_request(
    task_id: BoardTaskId,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Task<Result<()>> {
    let store = TaskBoardStore::global(cx);
    let Some(task) = store.read(cx).task(task_id).cloned() else {
        return Task::ready(Err(anyhow!("task no longer exists")));
    };
    let Some(branch_name) = task.branch_name.clone() else {
        return Task::ready(Err(anyhow!("start the task first to create its branch")));
    };
    // Multi-project tasks publish the primary project's branch; other repos'
    // branches can be pushed from their own worktrees.
    let Some(worktree_path) = task.worktree_path() else {
        return Task::ready(Err(anyhow!(
            "the task's worktree was cleaned up; reopen the task first"
        )));
    };
    let group_key = store
        .read(cx)
        .project(task.project_id)
        .map(|project| project.group_key());
    let Some(multi_workspace) = workspace.multi_workspace().cloned() else {
        return Task::ready(Err(anyhow!("task board requires a windowed workspace")));
    };

    cx.spawn_in(window, async move |source_workspace, cx| {
        let multi_workspace = multi_workspace.upgrade().context("window was closed")?;
        let task_workspace = multi_workspace
            .update_in(cx, |multi_workspace, window, cx| {
                multi_workspace.find_or_create_workspace(
                    PathList::new(&[worktree_path]),
                    None,
                    group_key,
                    |_, _, _| Task::ready(Ok(None)),
                    None,
                    OpenMode::Add,
                    Some(source_workspace.clone()),
                    window,
                    cx,
                )
            })?
            .await?;

        let repository = wait_for_repository(&task_workspace, cx).await?;

        // Look up the task branch's upstream to decide whether the push
        // needs to publish it.
        let branches = repository
            .update(cx, |repository, _| repository.branches())
            .await
            .map_err(|_| anyhow!("branch scan was canceled"))??;
        let branch = branches
            .branches
            .iter()
            .find(|branch| branch.name() == branch_name)
            .cloned()
            .with_context(|| format!("branch {branch_name} no longer exists in the worktree"))?;

        let push_options = match &branch.upstream {
            Some(upstream) if matches!(upstream.tracking, UpstreamTracking::Tracked(_)) => None,
            _ => Some(PushOptions::SetUpstream),
        };
        let remote_branch_name = branch
            .upstream
            .as_ref()
            .filter(|upstream| matches!(upstream.tracking, UpstreamTracking::Tracked(_)))
            .and_then(|upstream| upstream.branch_name())
            .unwrap_or(&branch_name)
            .to_string();

        let remotes = repository
            .update(cx, |repository, _| {
                repository.get_remotes(Some(branch_name.clone()), true)
            })
            .await
            .map_err(|_| anyhow!("remote scan was canceled"))??;
        let remote = remotes.first().cloned().context(
            "no git remote configured; add a remote to publish the branch",
        )?;

        let askpass_delegate = build_askpass_delegate(
            &source_workspace,
            format!("git push {}", remote.name),
            cx,
        )?;

        repository
            .update(cx, |repository, cx| {
                repository.push(
                    SharedString::from(branch_name.clone()),
                    SharedString::from(remote_branch_name.clone()),
                    remote.name.clone(),
                    push_options,
                    askpass_delegate,
                    cx,
                )
            })
            .await
            .map_err(|_| anyhow!("push was canceled"))??;

        let pr_url = cx.update(|_, cx| {
            let (remote_origin_url, remote_upstream_url) = repository.read_with(cx, |repository, _| {
                (
                    repository.remote_origin_url.clone(),
                    repository.remote_upstream_url.clone(),
                )
            });
            let remote_url = if remote.name.as_ref() == "upstream" {
                remote_upstream_url.or(remote_origin_url)
            } else {
                remote_origin_url.or(remote_upstream_url)
            }
            .context("the remote has no URL configured")?;

            let provider_registry = git::GitHostingProviderRegistry::global(cx);
            let (provider, parsed_remote) =
                git::parse_git_remote_url(provider_registry, &remote_url)
                    .with_context(|| format!("unsupported remote URL: {remote_url}"))?;
            let url = provider
                .build_create_pull_request_url(&parsed_remote, &remote_branch_name)
                .context("unable to construct a pull request URL for this git host")?;

            cx.open_url(url.as_str());
            anyhow::Ok(url.to_string())
        })??;

        cx.update(|_, cx| {
            TaskBoardStore::global(cx).update(cx, |store, cx| {
                store.update_task(task_id, |task| task.pr_url = Some(pr_url.clone()), cx);
                let needs_move = store
                    .task(task_id)
                    .is_some_and(|task| task.status != TaskStatus::InReview);
                if needs_move {
                    store.move_task(task_id, TaskStatus::InReview, usize::MAX, cx);
                }
            });
            // Pick up any PR that already exists for the branch; the
            // periodic board refresh catches the one being created now.
            crate::pr_sync::refresh_task_prs(task_id, cx).detach_and_log_err(cx);
        })?;
        Ok(())
    })
}

/// The git store discovers repositories asynchronously after a workspace
/// opens; poll until the list is non-empty and stable across two reads so
/// multi-repo workspaces aren't scanned half-discovered.
async fn wait_for_repositories(
    workspace: &Entity<Workspace>,
    cx: &mut AsyncWindowContext,
) -> Result<Vec<Entity<Repository>>> {
    let read_repositories = |cx: &mut AsyncWindowContext| {
        workspace.read_with(cx, |workspace, cx| {
            workspace
                .project()
                .read(cx)
                .repositories(cx)
                .values()
                .cloned()
                .collect::<Vec<_>>()
        })
    };

    let mut previous_count = 0;
    for _ in 0..40 {
        let repositories = read_repositories(cx);
        if !repositories.is_empty() && repositories.len() == previous_count {
            return Ok(repositories);
        }
        previous_count = repositories.len();
        cx.background_executor()
            .timer(Duration::from_millis(250))
            .await;
    }
    let repositories = read_repositories(cx);
    anyhow::ensure!(
        !repositories.is_empty(),
        "no git repository detected in the task's project"
    );
    Ok(repositories)
}

/// The git store discovers repositories asynchronously after a workspace
/// opens, so poll briefly instead of failing on the first read.
async fn wait_for_repository(
    workspace: &Entity<Workspace>,
    cx: &mut AsyncWindowContext,
) -> Result<Entity<Repository>> {
    for _ in 0..40 {
        let repository = workspace.read_with(cx, |workspace, cx| {
            workspace
                .project()
                .read(cx)
                .repositories(cx)
                .values()
                .next()
                .cloned()
        });
        if let Some(repository) = repository {
            return Ok(repository);
        }
        cx.background_executor()
            .timer(Duration::from_millis(250))
            .await;
    }
    Err(anyhow!("no git repository detected in the task's worktree"))
}

fn build_askpass_delegate(
    workspace: &WeakEntity<Workspace>,
    operation: String,
    cx: &mut AsyncWindowContext,
) -> Result<AskPassDelegate> {
    let workspace = workspace.clone();
    let operation = SharedString::from(operation);
    let window = cx.window_handle();
    Ok(AskPassDelegate::new(
        &mut *cx,
        move |prompt, tx, cx| {
            window
                .update(cx, |_, window, cx| {
                    workspace
                        .update(cx, |workspace, cx| {
                            workspace.toggle_modal(window, cx, |window, cx| {
                                AskPassModal::new(operation.clone(), prompt.into(), tx, window, cx)
                            });
                        })
                        .ok();
                })
                .ok();
        },
    ))
}

/// Change a task's status, routing moves to Done/Cancelled through the
/// finish flow (session archival + restorable worktree cleanup) when the
/// task has a worktree.
pub fn request_status_change(
    task_id: BoardTaskId,
    status: TaskStatus,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Task<Result<()>> {
    let store = TaskBoardStore::global(cx);
    let needs_finish_flow = matches!(status, TaskStatus::Done | TaskStatus::Cancelled)
        && store
            .read(cx)
            .task(task_id)
            .is_some_and(|task| task.has_worktree());

    if needs_finish_flow {
        finish_task(task_id, status, workspace, window, cx)
    } else {
        store.update(cx, |store, cx| {
            store.move_task(task_id, status, usize::MAX, cx);
        });
        Task::ready(Ok(()))
    }
}

/// Finish a task: optionally confirm when sessions are running, archive its
/// sessions, save the worktree's git state restorably, remove the worktree,
/// and move the task to the target status.
pub fn finish_task(
    task_id: BoardTaskId,
    status: TaskStatus,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Task<Result<()>> {
    let store = TaskBoardStore::global(cx);
    let Some(task) = store.read(cx).task(task_id).cloned() else {
        return Task::ready(Err(anyhow!("task no longer exists")));
    };
    let worktree_paths: Vec<PathBuf> = task.worktree_paths.ordered_paths().cloned().collect();
    if worktree_paths.is_empty() {
        store.update(cx, |store, cx| {
            store.move_task(task_id, status, usize::MAX, cx);
        });
        return Task::ready(Ok(()));
    }
    if task.all_project_ids().any(|project_id| {
        store
            .read(cx)
            .project(project_id)
            .is_some_and(|project| project.remote_connection.is_some())
    }) {
        return Task::ready(Err(anyhow!(
            "finishing tasks in remote projects is not supported yet"
        )));
    }

    let sessions = store.read(cx).sessions_for_task(task_id);
    let running_session_count = sessions
        .iter()
        .filter(|info| info.runtime != SessionRuntimeState::Dormant)
        .count();
    let cleanup_by_default = TaskBoardSettings::get_global(cx).cleanup_worktree_on_done;

    let confirmation = if running_session_count > 0 {
        Some(window.prompt(
            gpui::PromptLevel::Warning,
            &format!(
                "This task has {running_session_count} running session{}.",
                if running_session_count == 1 { "" } else { "s" }
            ),
            Some(
                "Archiving saves the worktree's uncommitted changes restorably \
                 and closes the task's sessions.",
            ),
            &["Archive Sessions & Clean Up", "Keep Worktree", "Cancel"],
            cx,
        ))
    } else {
        None
    };

    let group_key = store
        .read(cx)
        .project(task.project_id)
        .map(|project| project.group_key());
    let multi_workspace = workspace.multi_workspace().cloned();

    cx.spawn_in(window, async move |source_workspace, cx| {
        let cleanup = match confirmation {
            Some(answer) => match answer.await {
                Ok(0) => true,
                Ok(1) => false,
                _ => return Ok(()),
            },
            None => cleanup_by_default,
        };

        if cleanup {
            // Archive every session first so no agent is left running in a
            // directory that is about to disappear.
            cx.update(|window, cx| {
                let store = TaskBoardStore::global(cx);
                let sessions = store.read(cx).sessions_for_task(task_id);
                for info in sessions {
                    if !info.session.archived {
                        session_manager::archive_session(info.session.session_id, window, cx);
                    }
                }
            })?;

            for worktree_path in worktree_paths {
                // A worktree that an earlier, partially-failed finish already
                // archived and removed just needs dropping from the task.
                let already_archived = cx.update(|_, cx| {
                    TaskBoardStore::global(cx)
                        .read(cx)
                        .archived_worktrees_for_task(task_id)
                        .any(|row| row.worktree_path == worktree_path)
                })?;
                if !already_archived || worktree_path.exists() {
                    cleanup_task_worktree(
                        task_id,
                        worktree_path.clone(),
                        group_key.clone(),
                        multi_workspace.clone(),
                        source_workspace.clone(),
                        cx,
                    )
                    .await?;
                }

                // Drop the path from the task as soon as it is handled so a
                // failure on a later worktree can be retried without
                // re-archiving this one.
                cx.update(|_, cx| {
                    TaskBoardStore::global(cx).update(cx, |store, cx| {
                        store.update_task(
                            task_id,
                            |task| {
                                let remaining: Vec<PathBuf> = task
                                    .worktree_paths
                                    .ordered_paths()
                                    .filter(|path| **path != worktree_path)
                                    .cloned()
                                    .collect();
                                task.worktree_paths = PathList::new(&remaining);
                            },
                            cx,
                        );
                    });
                })?;
            }
        }

        cx.update(|_, cx| {
            TaskBoardStore::global(cx).update(cx, |store, cx| {
                store.move_task(task_id, status, usize::MAX, cx);
            });
        })?;
        Ok(())
    })
}

async fn cleanup_task_worktree(
    task_id: BoardTaskId,
    worktree_path: PathBuf,
    group_key: Option<ProjectGroupKey>,
    multi_workspace: Option<WeakEntity<workspace::MultiWorkspace>>,
    source_workspace: WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    let multi_workspace = multi_workspace
        .context("task board requires a windowed workspace")?
        .upgrade()
        .context("window was closed")?;

    // The archive plan needs the worktree loaded in an open project; open
    // its workspace in the background if necessary.
    let plan = {
        let workspaces =
            multi_workspace.read_with(cx, |multi_workspace, _| {
            multi_workspace.workspaces().cloned().collect::<Vec<_>>()
        });
        cx.update(|_, cx| build_root_plan(&worktree_path, None, &workspaces, cx))?
    };
    let plan = match plan {
        Some(plan) => plan,
        None => {
            let task_workspace = multi_workspace
                .update_in(cx, |multi_workspace, window, cx| {
                    multi_workspace.find_or_create_workspace(
                        PathList::new(std::slice::from_ref(&worktree_path)),
                        None,
                        group_key,
                        |_, _, _| Task::ready(Ok(None)),
                        None,
                        OpenMode::Add,
                        Some(source_workspace.clone()),
                        window,
                        cx,
                    )
                })?
                .await?;
            wait_for_repository(&task_workspace, cx).await?;
            let workspaces =
                multi_workspace.read_with(cx, |multi_workspace, _| {
            multi_workspace.workspaces().cloned().collect::<Vec<_>>()
        });
            cx.update(|_, cx| build_root_plan(&worktree_path, None, &workspaces, cx))?
                .context(
                    "the worktree could not be prepared for archival \
                     (it may not be a Zed-created linked worktree)",
                )?
        }
    };

    // Save the worktree's git state as two protected WIP commits, mirroring
    // Zed's thread archive flow but recording it against the task.
    let original_commit_hash = plan
        .worktree_repo
        .update(cx, |repository, _| repository.head_sha())
        .await
        .map_err(|_| anyhow!("head_sha was canceled"))?
        .context("failed to read the worktree's HEAD")?
        .context("the worktree has no HEAD commit")?;
    let (staged_commit_hash, unstaged_commit_hash) = plan
        .worktree_repo
        .update(cx, |repository, _| repository.create_archive_checkpoint())
        .await
        .map_err(|_| anyhow!("archive checkpoint was canceled"))??;

    let ref_name = task_board_archive_ref_name(task_id, &worktree_path);
    let (main_repo, _temp_project) = find_or_create_repository(&plan.main_repo_path, None, cx)
        .await
        .context("could not open the main repository to protect the archived state")?;
    main_repo
        .update(cx, |repository, _| {
            repository.update_ref(ref_name.clone(), unstaged_commit_hash.clone())
        })
        .await
        .map_err(|_| anyhow!("ref creation was canceled"))??;

    let branch_name = plan.branch_name.clone();
    let row = TaskBoardArchivedWorktree {
        task_id,
        worktree_path: worktree_path.clone(),
        main_repo_path: plan.main_repo_path.clone(),
        branch_name,
        staged_commit_hash,
        unstaged_commit_hash,
        original_commit_hash,
        ref_name,
    };
    cx.update(|_, cx| {
        TaskBoardStore::global(cx).update(cx, |store, cx| {
            store.save_archived_worktree(row, cx);
        });
    })?;

    remove_root(plan, cx).await?;
    Ok(())
}

fn task_board_archive_ref_name(task_id: BoardTaskId, worktree_path: &std::path::Path) -> String {
    let directory_name = worktree_path
        .file_name()
        .map(|name| slugify(&name.to_string_lossy()))
        .unwrap_or_else(|| "worktree".to_string());
    format!(
        "refs/task-board-archived/{}-{directory_name}",
        task_id.to_key_string()
    )
}

/// Reopen a finished task: recreate its archived worktree(s) with their
/// uncommitted state and move it back to In Progress.
pub fn reopen_task(
    task_id: BoardTaskId,
    _workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Task<Result<()>> {
    let store = TaskBoardStore::global(cx);
    let Some(task) = store.read(cx).task(task_id).cloned() else {
        return Task::ready(Err(anyhow!("task no longer exists")));
    };
    if task.has_worktree() {
        return Task::ready(Err(anyhow!("this task already has a worktree")));
    }
    let rows: Vec<TaskBoardArchivedWorktree> = store
        .read(cx)
        .archived_worktrees_for_task(task_id)
        .cloned()
        .collect();
    if rows.is_empty() {
        return Task::ready(Err(anyhow!("this task has no archived worktree to restore")));
    }

    cx.spawn_in(window, async move |_, cx| {
        let mut restored_paths = Vec::new();
        for row in &rows {
            let archived = ArchivedGitWorktree {
                id: 0,
                worktree_path: row.worktree_path.clone(),
                main_repo_path: row.main_repo_path.clone(),
                branch_name: row.branch_name.clone(),
                staged_commit_hash: row.staged_commit_hash.clone(),
                unstaged_commit_hash: row.unstaged_commit_hash.clone(),
                original_commit_hash: row.original_commit_hash.clone(),
            };
            let path = restore_worktree_via_git(&archived, None, cx).await?;
            restored_paths.push(path);
        }

        // Only once every worktree is restored on disk can the protective
        // refs go; deleting them per-row would leave earlier rows' checkpoint
        // commits unprotected if a later restore fails and gets retried.
        for row in &rows {
            if let Ok((main_repo, _temp_project)) =
                find_or_create_repository(&row.main_repo_path, None, cx).await
            {
                main_repo
                    .update(cx, |repository, _| repository.delete_ref(row.ref_name.clone()))
                    .await
                    .ok();
            }
        }

        cx.update(|_, cx| {
            TaskBoardStore::global(cx).update(cx, |store, cx| {
                store.update_task(
                    task_id,
                    |task| {
                        task.worktree_paths = PathList::new(&restored_paths);
                        task.archived = false;
                    },
                    cx,
                );
                store.clear_archived_worktrees(task_id, cx);
                store.move_task(task_id, TaskStatus::InProgress, usize::MAX, cx);
            });
        })?;
        Ok(())
    })
}

/// Delete a task after a confirmation that spells out what is lost: records
/// always; archived uncommitted changes become unrecoverable (their
/// GC-protection refs are removed best-effort); live worktrees stay on disk.
pub fn delete_task(
    task_id: BoardTaskId,
    _workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Task<Result<()>> {
    let store = TaskBoardStore::global(cx);
    let Some(task) = store.read(cx).task(task_id).cloned() else {
        return Task::ready(Ok(()));
    };
    let archived_rows: Vec<TaskBoardArchivedWorktree> = store
        .read(cx)
        .archived_worktrees_for_task(task_id)
        .cloned()
        .collect();

    let mut detail =
        String::from("The task, its tags, and its session records will be permanently removed.");
    if task.has_worktree() {
        detail.push_str(
            " Its worktrees and branch stay on disk; remove them with git if you \
             no longer need them.",
        );
    }
    if !archived_rows.is_empty() {
        detail.push_str(
            " The uncommitted changes archived when this task was finished will \
             become unrecoverable.",
        );
    }

    let answer = window.prompt(
        gpui::PromptLevel::Warning,
        "Delete this task?",
        Some(&detail),
        &["Delete", "Cancel"],
        cx,
    );

    cx.spawn_in(window, async move |_, cx| {
        if answer.await != Ok(0) {
            return Ok(());
        }

        // The archived checkpoints are unreachable once the task's records
        // are gone, so drop their GC-protection refs (best-effort).
        for row in &archived_rows {
            if let Ok((main_repo, _temp_project)) =
                find_or_create_repository(&row.main_repo_path, None, cx).await
            {
                main_repo
                    .update(cx, |repository, _| repository.delete_ref(row.ref_name.clone()))
                    .await
                    .ok();
            }
        }

        cx.update(|_, cx| {
            TaskBoardStore::global(cx).update(cx, |store, cx| {
                store.delete_task(task_id, cx);
            });
        })?;
        Ok(())
    })
}

/// Open (or activate) a workspace for the task's worktree in this window.
pub fn open_task_worktree(
    task_id: BoardTaskId,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Task<Result<()>> {
    let store = TaskBoardStore::global(cx);
    let Some(task) = store.read(cx).task(task_id).cloned() else {
        return Task::ready(Err(anyhow!("task no longer exists")));
    };
    if !task.has_worktree() {
        return Task::ready(Err(anyhow!("this task has no worktree yet")));
    }
    // Open every worktree the task spans as one multi-root workspace.
    let worktree_paths = task.worktree_paths.clone();
    let group_key = (task.extra_project_ids.is_empty())
        .then(|| {
            store
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
            .await?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_slugify() {
        assert_eq!(slugify("Fix crash in project panel"), "fix-crash-in-project-panel");
        assert_eq!(slugify("  Weird   spacing!!  "), "weird-spacing");
        assert_eq!(slugify("émojis 🎉 and ünicode"), "mojis-and-nicode");
        assert_eq!(slugify(""), "task");
        assert_eq!(slugify("!!!"), "task");
        assert!(slugify(&"long word ".repeat(20)).len() <= 40);
    }

    #[test]
    fn test_unique_slug() {
        let mut branches = HashSet::new();
        let mut worktrees = HashSet::new();
        assert_eq!(
            unique_slug("fix-bug", "task/", &branches, &worktrees),
            "fix-bug"
        );
        branches.insert("task/fix-bug".to_string());
        assert_eq!(
            unique_slug("fix-bug", "task/", &branches, &worktrees),
            "fix-bug-2"
        );
        branches.insert("task/fix-bug-2".to_string());
        assert_eq!(
            unique_slug("fix-bug", "task/", &branches, &worktrees),
            "fix-bug-3"
        );
        // A leftover worktree directory blocks its name even when no branch
        // exists for it.
        worktrees.insert("fix-bug-3".to_string());
        assert_eq!(
            unique_slug("fix-bug", "task/", &branches, &worktrees),
            "fix-bug-4"
        );
    }
}
