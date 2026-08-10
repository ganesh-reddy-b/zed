use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use chrono::{DateTime, Utc};
use gpui::{App, AppContext as _, Task};
use serde::Deserialize;
use util::ResultExt as _;

use crate::{BoardTaskId, PrState, TaskBoardStore, task_db::TaskPullRequest};

/// Minimum time between whole-board refreshes, so multiple open boards
/// don't hammer the GitHub CLI.
const REFRESH_DEBOUNCE: Duration = Duration::from_secs(60);

/// How often a visible board refreshes PR statuses.
pub(crate) const REFRESH_INTERVAL: Duration = Duration::from_secs(300);

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhPullRequest {
    number: i64,
    title: String,
    url: String,
    state: String,
    is_draft: bool,
    updated_at: Option<String>,
}

impl GhPullRequest {
    fn into_task_pr(self, task_id: BoardTaskId) -> TaskPullRequest {
        let state = match self.state.as_str() {
            "OPEN" if self.is_draft => PrState::Draft,
            "OPEN" => PrState::Open,
            "MERGED" => PrState::Merged,
            "CLOSED" => PrState::Closed,
            _ => PrState::Unknown,
        };
        TaskPullRequest {
            task_id,
            url: self.url,
            number: Some(self.number),
            title: Some(self.title),
            state,
            // The store re-applies each task's persisted detach flags when
            // it merges refreshed results.
            detached: false,
            updated_at: self
                .updated_at
                .as_deref()
                .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
                .map(|timestamp| timestamp.with_timezone(&Utc)),
        }
    }
}

/// Fetch the PRs for a task's branch via the GitHub CLI and update the
/// store. Errors are returned so manual refreshes can surface them;
/// background refreshes log them instead.
pub fn refresh_task_prs(task_id: BoardTaskId, cx: &App) -> Task<Result<()>> {
    let store = TaskBoardStore::global(cx);
    let Some(task) = store.read(cx).task(task_id).cloned() else {
        return Task::ready(Ok(()));
    };
    let Some(branch_name) = task.branch_name.clone() else {
        return Task::ready(Ok(()));
    };
    let repo_dir = store.read(cx).project(task.project_id).and_then(|project| {
        if project.remote_connection.is_some() {
            None
        } else {
            project.main_worktree_paths.ordered_paths().next().cloned()
        }
    });
    let Some(repo_dir) = repo_dir else {
        return Task::ready(Ok(()));
    };

    let fetch = cx.background_spawn(fetch_branch_prs(repo_dir, branch_name));
    cx.spawn(async move |cx| {
        let prs = fetch.await?;
        cx.update(|cx| {
            store.update(cx, |store, cx| {
                let prs = prs
                    .into_iter()
                    .map(|pr| pr.into_task_pr(task_id))
                    .collect();
                store.set_task_prs(task_id, prs, cx);
            });
        });
        Ok(())
    })
}

async fn fetch_branch_prs(repo_dir: PathBuf, branch_name: String) -> Result<Vec<GhPullRequest>> {
    let gh = find_gh().context(
        "the GitHub CLI (gh) was not found; install it (brew install gh) and \
         authenticate (gh auth login) to see pull request status",
    )?;
    let output = util::command::new_command(gh)
        .current_dir(&repo_dir)
        .args([
            "pr",
            "list",
            "--head",
            &branch_name,
            "--state",
            "all",
            "--json",
            "number,title,url,state,isDraft,updatedAt",
        ])
        .output()
        .await
        .context("failed to run `gh pr list`")?;
    anyhow::ensure!(
        output.status.success(),
        "gh pr list failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    serde_json::from_slice(&output.stdout).context("failed to parse `gh pr list` output")
}

/// Readiness of the GitHub CLI integration, checked when a board opens.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum GhSetupStatus {
    Ready,
    NotInstalled,
    NotAuthenticated,
}

pub(crate) async fn check_gh_setup() -> GhSetupStatus {
    let Some(gh) = find_gh() else {
        return GhSetupStatus::NotInstalled;
    };
    match util::command::new_command(gh)
        .args(["auth", "status"])
        .output()
        .await
    {
        Ok(output) if output.status.success() => GhSetupStatus::Ready,
        _ => GhSetupStatus::NotAuthenticated,
    }
}

pub(crate) fn gh_binary() -> Option<PathBuf> {
    find_gh()
}

fn find_gh() -> Option<PathBuf> {
    if let Ok(path) = which::which("gh") {
        return Some(path);
    }
    // GUI-launched Zed gets a minimal PATH that misses common install
    // locations (Homebrew et al), so probe those explicitly.
    let mut candidates = vec![
        PathBuf::from("/opt/homebrew/bin/gh"),
        PathBuf::from("/usr/local/bin/gh"),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(PathBuf::from(home).join(".local/bin/gh"));
    }
    candidates.into_iter().find(|path| path.is_file())
}

/// Refresh PRs for every task that could have one (has a branch, not
/// archived, local project). Debounced across boards; errors are logged.
pub fn refresh_all_prs(cx: &mut App) -> Task<()> {
    let store = TaskBoardStore::global(cx);
    let now = Instant::now();
    let should_run = store.update(cx, |store, _| store.begin_pr_refresh(now, REFRESH_DEBOUNCE));
    if !should_run {
        return Task::ready(());
    }

    // Skip everything quietly when gh isn't installed rather than logging
    // an error per task.
    if find_gh().is_none() {
        return Task::ready(());
    }

    let candidates: Vec<BoardTaskId> = store
        .read(cx)
        .tasks()
        .filter(|task| task.branch_name.is_some() && !task.archived)
        .map(|task| task.task_id)
        .collect();

    cx.spawn(async move |cx| {
        for task_id in candidates {
            let refresh = cx.update(|cx| refresh_task_prs(task_id, cx));
            refresh.await.log_err();
        }
    })
}

