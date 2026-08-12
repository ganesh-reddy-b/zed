use std::path::{Path, PathBuf};

use agent_ui::{AgentPanel, AgentPanelEvent, TerminalId};
use anyhow::Context as _;
use chrono::Utc;
use collections::{HashMap, HashSet};
use futures::{FutureExt, future::Shared};
use gpui::{
    App, AppContext as _, Context, Entity, EntityId, EventEmitter, Global, SharedString,
    Subscription, Task, WeakEntity, Window,
};
use project::ProjectGroupKey;
use remote::RemoteConnectionOptions;
use util::{ResultExt as _, path_list::PathList};

use crate::task_db::{
    BoardProject, BoardProjectId, BoardTask, BoardTaskId, TaskBoardArchivedWorktree, TaskBoardDb,
    TaskPullRequest, TaskSession, TaskSessionId, TaskStatus,
};

/// Minimum gap between adjacent sort orders before the column is
/// renormalized to whole numbers.
const MIN_SORT_ORDER_GAP: f64 = 1e-9;

/// Whether `path` is inside a git repository. `.git` may be a directory or,
/// for linked worktrees and submodules, a file.
pub(crate) fn path_is_in_git_repo(path: &Path) -> bool {
    path.ancestors()
        .any(|ancestor| ancestor.join(".git").exists())
}

struct GlobalTaskBoardStore(Entity<TaskBoardStore>);
impl Global for GlobalTaskBoardStore {}

#[cfg(any(test, feature = "test-support"))]
pub struct TestTaskBoardDbName(pub String);
#[cfg(any(test, feature = "test-support"))]
impl Global for TestTaskBoardDbName {}

#[cfg(any(test, feature = "test-support"))]
impl TestTaskBoardDbName {
    pub fn global(cx: &App) -> String {
        cx.try_global::<Self>()
            .map(|global| global.0.clone())
            .unwrap_or_else(|| {
                let thread = std::thread::current();
                let test_name = thread.name().unwrap_or("unknown_test");
                format!("TASK_BOARD_DB_{}", test_name)
            })
    }
}

/// Transient, per-run state of a session. Never persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SessionRuntimeState {
    /// No live terminal backs this session.
    #[default]
    Dormant,
    Running,
    /// The session's terminal rang the bell while unfocused; the agent is
    /// likely waiting for input.
    NeedsAttention,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TaskSessionInfo {
    pub session: TaskSession,
    pub runtime: SessionRuntimeState,
}

/// Filters applied by board surfaces when listing tasks.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BoardFilter {
    /// Show only tasks belonging to any of these projects; `None` shows all.
    pub projects: Option<Vec<BoardProjectId>>,
    pub tags: Vec<SharedString>,
    pub query: SharedString,
    pub show_archived: bool,
}

impl BoardFilter {
    fn matches(&self, task: &BoardTask) -> bool {
        if task.archived && !self.show_archived {
            return false;
        }
        if let Some(projects) = &self.projects
            && !task
                .all_project_ids()
                .any(|project_id| projects.contains(&project_id))
        {
            return false;
        }
        if !self
            .tags
            .iter()
            .all(|filter_tag| task.tags.iter().any(|tag| tag == filter_tag))
        {
            return false;
        }
        if !self.query.is_empty() {
            let query = self.query.to_lowercase();
            if !task.title.to_lowercase().contains(&query) {
                return false;
            }
        }
        true
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum TaskBoardStoreEvent {
    Reloaded,
    TaskChanged(BoardTaskId),
    TaskRemoved(BoardTaskId),
    SessionChanged(TaskSessionId),
    SessionRuntimeChanged(TaskSessionId),
    ProjectChanged(BoardProjectId),
}

enum DbOperation {
    UpsertProject(BoardProject),
    UpsertTask(BoardTask),
    DeleteTask(BoardTaskId),
    ReplaceTags(BoardTaskId, Vec<String>),
    ReplaceTaskProjects(BoardTaskId, Vec<BoardProjectId>),
    UpsertSession(TaskSession),
    DeleteSession(TaskSessionId),
    UpdateSortOrders(Vec<(BoardTaskId, f64)>),
    UpsertArchivedWorktree(TaskBoardArchivedWorktree),
    DeleteArchivedWorktrees(BoardTaskId),
    ReplaceTaskPrs(BoardTaskId, Vec<TaskPullRequest>),
}

pub struct TaskBoardStore {
    db: TaskBoardDb,
    projects: HashMap<BoardProjectId, BoardProject>,
    tasks: HashMap<BoardTaskId, BoardTask>,
    sessions: HashMap<TaskSessionId, TaskSession>,
    archived_worktrees: HashMap<BoardTaskId, Vec<TaskBoardArchivedWorktree>>,
    prs: HashMap<BoardTaskId, Vec<TaskPullRequest>>,
    /// Local projects whose folders are not inside a git repository, checked
    /// on disk in the background. Transient; recomputed on reload and
    /// registration.
    non_git_projects: HashSet<BoardProjectId>,
    tasks_by_project: HashMap<BoardProjectId, HashSet<BoardTaskId>>,
    sessions_by_task: HashMap<BoardTaskId, Vec<TaskSessionId>>,
    session_by_terminal: HashMap<String, TaskSessionId>,
    runtime: HashMap<TaskSessionId, SessionRuntimeState>,
    monitored_panels: Vec<WeakEntity<AgentPanel>>,
    panel_sessions: HashMap<EntityId, HashSet<TaskSessionId>>,
    last_pr_refresh: Option<std::time::Instant>,
    reload_task: Option<Shared<Task<()>>>,
    pending_ops_tx: async_channel::Sender<DbOperation>,
    _db_operations_task: Task<()>,
    _panel_subscriptions: Vec<Subscription>,
}

impl EventEmitter<TaskBoardStoreEvent> for TaskBoardStore {}

impl TaskBoardStore {
    #[cfg(not(any(test, feature = "test-support")))]
    pub fn init_global(cx: &mut App) {
        if cx.has_global::<GlobalTaskBoardStore>() {
            return;
        }

        let db = TaskBoardDb::global(cx);
        let store = cx.new(|cx| Self::new(db, cx));
        cx.set_global(GlobalTaskBoardStore(store));
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn init_global(cx: &mut App) {
        let db_name = TestTaskBoardDbName::global(cx);
        let db = gpui::block_on(db::open_test_db::<TaskBoardDb>(&db_name));
        let store = cx.new(|cx| Self::new(TaskBoardDb(db), cx));
        cx.set_global(GlobalTaskBoardStore(store));
    }

    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalTaskBoardStore>()
            .map(|store| store.0.clone())
    }

    pub fn global(cx: &App) -> Entity<Self> {
        cx.global::<GlobalTaskBoardStore>().0.clone()
    }

    fn new(db: TaskBoardDb, cx: &mut Context<Self>) -> Self {
        let (tx, rx) = async_channel::unbounded();
        let _db_operations_task = cx.background_spawn({
            let db = db.clone();
            async move {
                while let Ok(operation) = rx.recv().await {
                    Self::apply_db_operation(&db, operation).await.log_err();
                }
            }
        });

        let mut this = Self {
            db,
            projects: HashMap::default(),
            tasks: HashMap::default(),
            sessions: HashMap::default(),
            archived_worktrees: HashMap::default(),
            prs: HashMap::default(),
            non_git_projects: HashSet::default(),
            tasks_by_project: HashMap::default(),
            sessions_by_task: HashMap::default(),
            session_by_terminal: HashMap::default(),
            runtime: HashMap::default(),
            monitored_panels: Vec::new(),
            panel_sessions: HashMap::default(),
            last_pr_refresh: None,
            reload_task: None,
            pending_ops_tx: tx,
            _db_operations_task,
            _panel_subscriptions: Vec::new(),
        };
        this.reload(cx);
        this
    }

    async fn apply_db_operation(db: &TaskBoardDb, operation: DbOperation) -> anyhow::Result<()> {
        match operation {
            DbOperation::UpsertProject(project) => db.save_project(project).await,
            DbOperation::UpsertTask(task) => db.save_task(task).await,
            DbOperation::DeleteTask(task_id) => db.delete_task(task_id).await,
            DbOperation::ReplaceTags(task_id, tags) => db.replace_tags(task_id, tags).await,
            DbOperation::ReplaceTaskProjects(task_id, project_ids) => {
                db.replace_task_projects(task_id, project_ids).await
            }
            DbOperation::UpsertSession(session) => db.save_session(session).await,
            DbOperation::DeleteSession(session_id) => db.delete_session(session_id).await,
            DbOperation::UpdateSortOrders(orders) => db.update_sort_orders(orders).await,
            DbOperation::UpsertArchivedWorktree(row) => db.save_archived_worktree(row).await,
            DbOperation::DeleteArchivedWorktrees(task_id) => {
                db.delete_archived_worktrees(task_id).await
            }
            DbOperation::ReplaceTaskPrs(task_id, prs) => db.replace_task_prs(task_id, prs).await,
        }
    }

    fn enqueue(&self, operation: DbOperation) {
        self.pending_ops_tx.try_send(operation).log_err();
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let db = self.db.clone();
        self.reload_task = Some(
            cx.spawn(async move |this, cx| {
                let rows = cx
                    .background_spawn(async move {
                        let projects = db.list_projects().context("list board projects")?;
                        let tasks = db.list_tasks().context("list board tasks")?;
                        let tags = db.list_tags().context("list board task tags")?;
                        let task_projects = db
                            .list_task_projects()
                            .context("list board task projects")?;
                        let sessions = db.list_sessions().context("list board sessions")?;
                        let archived_worktrees = db
                            .list_archived_worktrees()
                            .context("list board archived worktrees")?;
                        let prs = db.list_prs().context("list board task PRs")?;
                        anyhow::Ok((
                            projects,
                            tasks,
                            tags,
                            task_projects,
                            sessions,
                            archived_worktrees,
                            prs,
                        ))
                    })
                    .await
                    .log_err();

                let Some((
                    projects,
                    mut tasks,
                    tags,
                    task_projects,
                    sessions,
                    archived_worktrees,
                    prs,
                )) = rows
                else {
                    return;
                };

                let mut tags_by_task: HashMap<BoardTaskId, Vec<SharedString>> = HashMap::default();
                for (task_id, tag) in tags {
                    tags_by_task.entry(task_id).or_default().push(tag.into());
                }
                let mut projects_by_task: HashMap<BoardTaskId, Vec<BoardProjectId>> =
                    HashMap::default();
                for (task_id, project_id) in task_projects {
                    projects_by_task.entry(task_id).or_default().push(project_id);
                }
                for task in &mut tasks {
                    if let Some(task_tags) = tags_by_task.remove(&task.task_id) {
                        task.tags = task_tags;
                    }
                    if let Some(mut extra) = projects_by_task.remove(&task.task_id) {
                        extra.retain(|project_id| *project_id != task.project_id);
                        task.extra_project_ids = extra;
                    }
                }

                this.update(cx, |this, cx| {
                    this.projects.clear();
                    this.tasks.clear();
                    this.sessions.clear();
                    this.archived_worktrees.clear();
                    this.prs.clear();
                    this.tasks_by_project.clear();
                    this.sessions_by_task.clear();
                    this.session_by_terminal.clear();

                    for project in projects {
                        this.projects.insert(project.project_id, project);
                    }
                    for task in tasks {
                        this.cache_task(task);
                    }
                    for session in sessions {
                        this.cache_session(session);
                    }
                    for row in archived_worktrees {
                        this.archived_worktrees
                            .entry(row.task_id)
                            .or_default()
                            .push(row);
                    }
                    for pr in prs {
                        this.prs.entry(pr.task_id).or_default().push(pr);
                    }

                    this.refresh_project_git_states(cx);
                    cx.emit(TaskBoardStoreEvent::Reloaded);
                    cx.notify();
                })
                .ok();
            })
            .shared(),
        );
    }

    pub fn reload_task(&self) -> Shared<Task<()>> {
        self.reload_task
            .clone()
            .unwrap_or_else(|| Task::ready(()).shared())
    }

    fn cache_task(&mut self, task: BoardTask) {
        self.tasks_by_project
            .entry(task.project_id)
            .or_default()
            .insert(task.task_id);
        self.tasks.insert(task.task_id, task);
    }

    fn cache_session(&mut self, session: TaskSession) {
        let ids = self.sessions_by_task.entry(session.task_id).or_default();
        if !ids.contains(&session.session_id) {
            ids.push(session.session_id);
        }
        if let Some(terminal_id) = &session.terminal_id {
            self.session_by_terminal
                .insert(terminal_id.clone(), session.session_id);
        }
        self.sessions.insert(session.session_id, session);
    }

    // --- Queries ---

    pub fn projects(&self) -> impl Iterator<Item = &BoardProject> + '_ {
        self.projects.values()
    }

    pub fn project(&self, project_id: BoardProjectId) -> Option<&BoardProject> {
        self.projects.get(&project_id)
    }

    /// Whether the project's folders lack a git repository. Tasks in such
    /// projects run directly in the project folder — no worktree or branch —
    /// and board surfaces mark them with a "no git" indicator.
    pub fn project_is_non_git(&self, project_id: BoardProjectId) -> bool {
        self.non_git_projects.contains(&project_id)
    }

    /// Re-derive which local projects lack a git repository, checking the
    /// filesystem in the background and emitting `ProjectChanged` for any
    /// project whose state flipped (e.g. after `git init`).
    fn refresh_project_git_states(&mut self, cx: &mut Context<Self>) {
        let candidates: Vec<(BoardProjectId, Vec<PathBuf>)> = self
            .projects
            .values()
            .filter(|project| project.remote_connection.is_none())
            .map(|project| {
                (
                    project.project_id,
                    project
                        .main_worktree_paths
                        .ordered_paths()
                        .cloned()
                        .collect(),
                )
            })
            .collect();
        cx.spawn(async move |this, cx| {
            let non_git: HashSet<BoardProjectId> = cx
                .background_spawn(async move {
                    candidates
                        .into_iter()
                        .filter(|(_, paths)| {
                            !paths.is_empty()
                                && !paths.iter().any(|path| path_is_in_git_repo(path))
                        })
                        .map(|(project_id, _)| project_id)
                        .collect()
                })
                .await;
            this.update(cx, |this, cx| {
                if this.non_git_projects != non_git {
                    let changed: Vec<BoardProjectId> = this
                        .non_git_projects
                        .symmetric_difference(&non_git)
                        .copied()
                        .collect();
                    this.non_git_projects = non_git;
                    for project_id in changed {
                        cx.emit(TaskBoardStoreEvent::ProjectChanged(project_id));
                    }
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    pub fn task(&self, task_id: BoardTaskId) -> Option<&BoardTask> {
        self.tasks.get(&task_id)
    }

    pub fn tasks(&self) -> impl Iterator<Item = &BoardTask> + '_ {
        self.tasks.values()
    }

    /// Let the next whole-board PR refresh run immediately (e.g. right
    /// after the GitHub CLI setup completes).
    pub(crate) fn reset_pr_refresh_debounce(&mut self) {
        self.last_pr_refresh = None;
    }

    /// Claim a whole-board PR refresh slot, debouncing concurrent boards.
    pub(crate) fn begin_pr_refresh(
        &mut self,
        now: std::time::Instant,
        debounce: std::time::Duration,
    ) -> bool {
        match self.last_pr_refresh {
            Some(last) if now.duration_since(last) < debounce => false,
            _ => {
                self.last_pr_refresh = Some(now);
                true
            }
        }
    }

    pub fn session(&self, session_id: TaskSessionId) -> Option<&TaskSession> {
        self.sessions.get(&session_id)
    }

    pub fn session_for_terminal(&self, terminal_key: &str) -> Option<&TaskSession> {
        self.session_by_terminal
            .get(terminal_key)
            .and_then(|id| self.sessions.get(id))
    }

    /// Tasks in the given column, filtered and sorted by manual order.
    pub fn tasks_for_column(&self, status: TaskStatus, filter: &BoardFilter) -> Vec<&BoardTask> {
        let mut tasks: Vec<_> = self
            .tasks
            .values()
            .filter(|task| task.status == status && filter.matches(task))
            .collect();
        tasks.sort_by(|a, b| {
            a.sort_order
                .total_cmp(&b.sort_order)
                .then_with(|| a.created_at.cmp(&b.created_at))
        });
        tasks
    }

    pub fn sessions_for_task(&self, task_id: BoardTaskId) -> Vec<TaskSessionInfo> {
        self.sessions_by_task
            .get(&task_id)
            .into_iter()
            .flatten()
            .filter_map(|session_id| self.sessions.get(session_id))
            .map(|session| TaskSessionInfo {
                session: session.clone(),
                runtime: self
                    .runtime
                    .get(&session.session_id)
                    .copied()
                    .unwrap_or_default(),
            })
            .collect()
    }

    pub fn session_runtime(&self, session_id: TaskSessionId) -> SessionRuntimeState {
        self.runtime
            .get(&session_id)
            .copied()
            .unwrap_or(SessionRuntimeState::Dormant)
    }

    pub fn task_needs_attention(&self, task_id: BoardTaskId) -> bool {
        self.sessions_by_task
            .get(&task_id)
            .into_iter()
            .flatten()
            .any(|session_id| {
                self.runtime.get(session_id) == Some(&SessionRuntimeState::NeedsAttention)
            })
    }

    pub fn archived_worktrees_for_task(
        &self,
        task_id: BoardTaskId,
    ) -> impl Iterator<Item = &TaskBoardArchivedWorktree> + '_ {
        self.archived_worktrees
            .get(&task_id)
            .into_iter()
            .flatten()
    }

    /// Pull requests discovered for the task's branch, ordered by number.
    pub fn prs_for_task(&self, task_id: BoardTaskId) -> &[TaskPullRequest] {
        self.prs
            .get(&task_id)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// Replace the task's PR records (from a `gh` refresh). Detached PRs
    /// keep their flag and are retained even when discovery no longer
    /// returns them, so they can't silently re-attach. No-op — and no event
    /// — when nothing changed, so periodic refreshes stay quiet.
    pub fn set_task_prs(
        &mut self,
        task_id: BoardTaskId,
        mut prs: Vec<TaskPullRequest>,
        cx: &mut Context<Self>,
    ) {
        if !self.tasks.contains_key(&task_id) {
            return;
        }
        let existing = self.prs.get(&task_id).map(Vec::as_slice).unwrap_or_default();
        for pr in &mut prs {
            pr.detached = existing
                .iter()
                .any(|existing| existing.url == pr.url && existing.detached);
        }
        for existing in existing {
            if existing.detached && !prs.iter().any(|pr| pr.url == existing.url) {
                prs.push(existing.clone());
            }
        }
        prs.sort_by(|a, b| a.number.cmp(&b.number).then_with(|| a.url.cmp(&b.url)));
        self.replace_task_prs(task_id, prs, cx);
    }

    /// Detach a PR from the task (hide it, surviving refreshes) or
    /// re-attach it.
    pub fn set_pr_detached(
        &mut self,
        task_id: BoardTaskId,
        url: &str,
        detached: bool,
        cx: &mut Context<Self>,
    ) {
        let mut prs = self
            .prs
            .get(&task_id)
            .map(Vec::as_slice)
            .unwrap_or_default()
            .to_vec();
        let Some(pr) = prs.iter_mut().find(|pr| pr.url == url) else {
            return;
        };
        pr.detached = detached;
        self.replace_task_prs(task_id, prs, cx);
    }

    fn replace_task_prs(
        &mut self,
        task_id: BoardTaskId,
        prs: Vec<TaskPullRequest>,
        cx: &mut Context<Self>,
    ) {
        let existing = self.prs.get(&task_id).map(Vec::as_slice).unwrap_or_default();
        if existing == prs.as_slice() {
            return;
        }
        if prs.is_empty() {
            self.prs.remove(&task_id);
        } else {
            self.prs.insert(task_id, prs.clone());
        }
        self.enqueue(DbOperation::ReplaceTaskPrs(task_id, prs));
        cx.emit(TaskBoardStoreEvent::TaskChanged(task_id));
        cx.notify();
    }

    /// The registered project matching the given group key, if any.
    pub fn project_for_group_key(&self, key: &ProjectGroupKey) -> Option<&BoardProject> {
        self.projects
            .values()
            .find(|project| project.group_key().matches(key))
    }

    // --- Mutations ---

    /// Register a project (repository) on the board, or return the existing
    /// registration matching the same group key.
    pub fn register_project(
        &mut self,
        main_worktree_paths: PathList,
        remote_connection: Option<RemoteConnectionOptions>,
        display_name: SharedString,
        cx: &mut Context<Self>,
    ) -> BoardProjectId {
        let key = ProjectGroupKey::new(remote_connection.clone(), main_worktree_paths.clone());
        if let Some(existing) = self.project_for_group_key(&key) {
            return existing.project_id;
        }

        let project = BoardProject {
            project_id: BoardProjectId::new(),
            main_worktree_paths,
            remote_connection,
            display_name,
            created_at: Utc::now(),
        };
        let project_id = project.project_id;
        self.projects.insert(project_id, project.clone());
        self.enqueue(DbOperation::UpsertProject(project));
        self.refresh_project_git_states(cx);
        cx.emit(TaskBoardStoreEvent::ProjectChanged(project_id));
        cx.notify();
        project_id
    }

    pub fn create_task(
        &mut self,
        project_id: BoardProjectId,
        title: SharedString,
        description: Option<String>,
        status: TaskStatus,
        cx: &mut Context<Self>,
    ) -> BoardTaskId {
        let sort_order = self.max_sort_order_in_column(status) + 1.0;
        let now = Utc::now();
        let task = BoardTask {
            task_id: BoardTaskId::new(),
            project_id,
            extra_project_ids: Vec::new(),
            title,
            description,
            status,
            sort_order,
            branch_name: None,
            base_branch_target: None,
            worktree_paths: PathList::new::<std::path::PathBuf>(&[]),
            pr_url: None,
            archived: false,
            tags: Vec::new(),
            created_at: now,
            updated_at: now,
        };
        let task_id = task.task_id;
        self.cache_task(task.clone());
        self.enqueue(DbOperation::UpsertTask(task));
        cx.emit(TaskBoardStoreEvent::TaskChanged(task_id));
        cx.notify();
        task_id
    }

    /// Apply arbitrary field changes to a task. Status and sort order should
    /// go through `move_task` instead so column ordering stays consistent.
    pub fn update_task(
        &mut self,
        task_id: BoardTaskId,
        update: impl FnOnce(&mut BoardTask),
        cx: &mut Context<Self>,
    ) {
        let Some(task) = self.tasks.get_mut(&task_id) else {
            return;
        };
        let old_tags = task.tags.clone();
        let old_extra_projects = task.extra_project_ids.clone();
        update(task);
        // The primary project is implied; keep the extras list free of it and
        // of duplicates no matter what the caller wrote.
        let primary = task.project_id;
        let mut seen = HashSet::default();
        task.extra_project_ids
            .retain(|project_id| *project_id != primary && seen.insert(*project_id));
        task.updated_at = Utc::now();
        let task = task.clone();

        if task.tags != old_tags {
            self.enqueue(DbOperation::ReplaceTags(
                task_id,
                task.tags.iter().map(ToString::to_string).collect(),
            ));
        }
        if task.extra_project_ids != old_extra_projects {
            self.enqueue(DbOperation::ReplaceTaskProjects(
                task_id,
                task.extra_project_ids.clone(),
            ));
        }
        self.enqueue(DbOperation::UpsertTask(task));
        cx.emit(TaskBoardStoreEvent::TaskChanged(task_id));
        cx.notify();
    }

    /// Move a task into `status` before `before` (or to the end when
    /// `None`). Resolving the anchor task against the unfiltered column here
    /// keeps drops correct while the board is filtered, where visual indices
    /// and column indices diverge.
    pub fn move_task_before(
        &mut self,
        task_id: BoardTaskId,
        status: TaskStatus,
        before: Option<BoardTaskId>,
        cx: &mut Context<Self>,
    ) {
        let index = before
            .and_then(|before_id| {
                self.ordered_column_without(status, task_id)
                    .iter()
                    .position(|(id, _)| *id == before_id)
            })
            .unwrap_or(usize::MAX);
        self.move_task(task_id, status, index, cx);
    }

    /// Move a task to `status` at position `index` within that column
    /// (indices are into the unfiltered, ordered column with the moved task
    /// removed).
    pub fn move_task(
        &mut self,
        task_id: BoardTaskId,
        status: TaskStatus,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        if !self.tasks.contains_key(&task_id) {
            return;
        }

        let column = self.ordered_column_without(status, task_id);
        let index = index.min(column.len());
        let prev = index
            .checked_sub(1)
            .and_then(|prev_index| column.get(prev_index))
            .map(|(_, order)| *order);
        let next = column.get(index).map(|(_, order)| *order);

        let sort_order = match (prev, next) {
            (None, None) => 1.0,
            (Some(prev), None) => prev + 1.0,
            (None, Some(next)) => next - 1.0,
            (Some(prev), Some(next)) => {
                if next - prev < MIN_SORT_ORDER_GAP {
                    self.renormalize_column(status, task_id, index, cx);
                    return;
                }
                (prev + next) / 2.0
            }
        };

        self.set_task_position(task_id, status, sort_order, cx);
    }

    fn set_task_position(
        &mut self,
        task_id: BoardTaskId,
        status: TaskStatus,
        sort_order: f64,
        cx: &mut Context<Self>,
    ) {
        let Some(task) = self.tasks.get_mut(&task_id) else {
            return;
        };
        task.status = status;
        task.sort_order = sort_order;
        task.updated_at = Utc::now();
        let task = task.clone();
        self.enqueue(DbOperation::UpsertTask(task));
        cx.emit(TaskBoardStoreEvent::TaskChanged(task_id));
        cx.notify();
    }

    /// Reassign whole-number sort orders to an entire column, inserting the
    /// moved task at `index`.
    fn renormalize_column(
        &mut self,
        status: TaskStatus,
        moved_task_id: BoardTaskId,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        let column = self.ordered_column_without(status, moved_task_id);
        let mut ordered_ids: Vec<BoardTaskId> = column.iter().map(|(id, _)| *id).collect();
        let index = index.min(ordered_ids.len());
        ordered_ids.insert(index, moved_task_id);

        let mut orders = Vec::with_capacity(ordered_ids.len());
        let mut moved_task_order = 1.0;
        for (position, id) in ordered_ids.iter().enumerate() {
            let sort_order = (position + 1) as f64;
            if *id == moved_task_id {
                moved_task_order = sort_order;
            } else if let Some(task) = self.tasks.get_mut(id) {
                task.sort_order = sort_order;
                orders.push((*id, sort_order));
            }
        }

        self.enqueue(DbOperation::UpdateSortOrders(orders));
        self.set_task_position(moved_task_id, status, moved_task_order, cx);
    }

    fn ordered_column_without(
        &self,
        status: TaskStatus,
        excluded: BoardTaskId,
    ) -> Vec<(BoardTaskId, f64)> {
        let mut column: Vec<_> = self
            .tasks
            .values()
            .filter(|task| task.status == status && task.task_id != excluded)
            .map(|task| (task.task_id, task.sort_order))
            .collect();
        column.sort_by(|(_, a), (_, b)| a.total_cmp(b));
        column
    }

    fn max_sort_order_in_column(&self, status: TaskStatus) -> f64 {
        self.tasks
            .values()
            .filter(|task| task.status == status)
            .map(|task| task.sort_order)
            .fold(0.0, f64::max)
    }

    pub fn set_task_archived(&mut self, task_id: BoardTaskId, archived: bool, cx: &mut Context<Self>) {
        self.update_task(task_id, |task| task.archived = archived, cx);
    }

    /// Permanently delete a task along with its tags, sessions, and archived
    /// worktree records.
    pub fn delete_task(&mut self, task_id: BoardTaskId, cx: &mut Context<Self>) {
        let Some(task) = self.tasks.remove(&task_id) else {
            return;
        };
        if let Some(ids) = self.tasks_by_project.get_mut(&task.project_id) {
            ids.remove(&task_id);
        }
        for session_id in self.sessions_by_task.remove(&task_id).unwrap_or_default() {
            if let Some(session) = self.sessions.remove(&session_id)
                && let Some(terminal_id) = &session.terminal_id
            {
                self.session_by_terminal.remove(terminal_id);
            }
            self.runtime.remove(&session_id);
        }
        self.archived_worktrees.remove(&task_id);
        self.prs.remove(&task_id);
        self.enqueue(DbOperation::DeleteTask(task_id));
        cx.emit(TaskBoardStoreEvent::TaskRemoved(task_id));
        cx.notify();
    }

    // --- Sessions ---

    pub fn upsert_session(&mut self, session: TaskSession, cx: &mut Context<Self>) {
        let session_id = session.session_id;
        if let Some(existing) = self.sessions.get(&session_id)
            && let Some(old_terminal_id) = &existing.terminal_id
            && existing.terminal_id != session.terminal_id
        {
            self.session_by_terminal.remove(old_terminal_id);
        }
        self.cache_session(session.clone());
        self.enqueue(DbOperation::UpsertSession(session));
        cx.emit(TaskBoardStoreEvent::SessionChanged(session_id));
        cx.notify();
    }

    /// Mark a session archived, keeping its record (agent, cwd, resume
    /// command) while dropping the link to the now-closed terminal.
    pub fn archive_session(&mut self, session_id: TaskSessionId, cx: &mut Context<Self>) {
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return;
        };
        if let Some(terminal_id) = session.terminal_id.take() {
            self.session_by_terminal.remove(&terminal_id);
        }
        session.archived = true;
        session.archived_at = Some(Utc::now());
        let session = session.clone();
        self.runtime.remove(&session_id);
        self.enqueue(DbOperation::UpsertSession(session));
        cx.emit(TaskBoardStoreEvent::SessionChanged(session_id));
        cx.notify();
    }

    pub fn delete_session(&mut self, session_id: TaskSessionId, cx: &mut Context<Self>) {
        let Some(session) = self.sessions.remove(&session_id) else {
            return;
        };
        if let Some(ids) = self.sessions_by_task.get_mut(&session.task_id) {
            ids.retain(|id| *id != session_id);
        }
        if let Some(terminal_id) = &session.terminal_id {
            self.session_by_terminal.remove(terminal_id);
        }
        self.runtime.remove(&session_id);
        self.enqueue(DbOperation::DeleteSession(session_id));
        cx.emit(TaskBoardStoreEvent::SessionChanged(session_id));
        cx.notify();
    }

    /// Update transient runtime state (running / needs-attention). Not
    /// persisted; emitted so every window's board can update badges.
    pub fn set_session_runtime(
        &mut self,
        session_id: TaskSessionId,
        state: SessionRuntimeState,
        cx: &mut Context<Self>,
    ) {
        if !self.sessions.contains_key(&session_id) {
            return;
        }
        let previous = if state == SessionRuntimeState::Dormant {
            self.runtime.remove(&session_id).unwrap_or_default()
        } else {
            self.runtime.insert(session_id, state).unwrap_or_default()
        };
        if previous != state {
            cx.emit(TaskBoardStoreEvent::SessionRuntimeChanged(session_id));
            cx.notify();
        }
    }

    // --- Agent panel monitoring ---

    /// Track an agent panel so sessions backed by its terminal threads get
    /// live runtime state (running / needs-attention) across all windows.
    pub fn monitor_agent_panel(&mut self, panel: Entity<AgentPanel>, cx: &mut Context<Self>) {
        let panel_id = panel.entity_id();
        self._panel_subscriptions.push(cx.subscribe(
            &panel,
            |this, panel, event: &AgentPanelEvent, cx| match event {
                AgentPanelEvent::EntryChanged
                | AgentPanelEvent::ActiveViewChanged
                | AgentPanelEvent::TerminalCloseRequested { .. } => {
                    this.sync_panel_sessions(&panel, cx);
                }
                AgentPanelEvent::ActiveViewFocused
                | AgentPanelEvent::ThreadInteracted { .. } => {}
            },
        ));
        self._panel_subscriptions
            .push(cx.observe_release(&panel, move |this, _, cx| {
                for session_id in this.panel_sessions.remove(&panel_id).unwrap_or_default() {
                    this.set_session_runtime(session_id, SessionRuntimeState::Dormant, cx);
                }
                this.monitored_panels
                    .retain(|panel| panel.entity_id() != panel_id);
            }));
        self.monitored_panels.push(panel.downgrade());

        // This runs from `observe_new` while the panel entity is still being
        // updated, so reading it here would panic with a double lease; defer
        // the initial sync until the current update finishes.
        let store = cx.weak_entity();
        let panel = panel.downgrade();
        cx.defer(move |cx| {
            if let Some((store, panel)) = store.upgrade().zip(panel.upgrade()) {
                store.update(cx, |store, cx| store.sync_panel_sessions(&panel, cx));
            }
        });
    }

    fn sync_panel_sessions(&mut self, panel: &Entity<AgentPanel>, cx: &mut Context<Self>) {
        let terminals = panel.read(cx).terminals(cx);

        let mut present = HashSet::default();
        let mut updates = Vec::new();
        for info in terminals {
            let Some(session_id) = self
                .session_by_terminal
                .get(&info.id.to_key_string())
                .copied()
            else {
                continue;
            };
            present.insert(session_id);
            let state = if info.has_notification {
                SessionRuntimeState::NeedsAttention
            } else {
                SessionRuntimeState::Running
            };
            updates.push((session_id, state));
        }

        let previous = self
            .panel_sessions
            .insert(panel.entity_id(), present.clone())
            .unwrap_or_default();
        for session_id in previous.difference(&present) {
            updates.push((*session_id, SessionRuntimeState::Dormant));
        }
        for (session_id, state) in updates {
            self.set_session_runtime(session_id, state, cx);
        }
    }

    /// Close the terminal thread backing a session, if it is live in a panel
    /// belonging to this window. Terminals live in other windows keep
    /// running until closed there.
    pub fn close_live_terminal(
        &mut self,
        terminal_id: TerminalId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.monitored_panels
            .retain(|panel| panel.upgrade().is_some());
        for panel in self.monitored_panels.clone() {
            let Some(panel) = panel.upgrade() else {
                continue;
            };
            if panel.read(cx).has_terminal(terminal_id) {
                panel.update(cx, |panel, cx| {
                    panel.close_terminal_without_activating_draft(terminal_id, window, cx);
                });
                return;
            }
        }
    }

    // --- Archived worktrees ---

    pub fn save_archived_worktree(
        &mut self,
        row: TaskBoardArchivedWorktree,
        cx: &mut Context<Self>,
    ) {
        let task_id = row.task_id;
        let rows = self.archived_worktrees.entry(task_id).or_default();
        rows.retain(|existing| existing.worktree_path != row.worktree_path);
        rows.push(row.clone());
        self.enqueue(DbOperation::UpsertArchivedWorktree(row));
        cx.emit(TaskBoardStoreEvent::TaskChanged(task_id));
        cx.notify();
    }

    pub fn clear_archived_worktrees(&mut self, task_id: BoardTaskId, cx: &mut Context<Self>) {
        if self.archived_worktrees.remove(&task_id).is_none() {
            return;
        }
        self.enqueue(DbOperation::DeleteArchivedWorktrees(task_id));
        cx.emit(TaskBoardStoreEvent::TaskChanged(task_id));
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use pretty_assertions::assert_eq;

    fn init_test(cx: &mut TestAppContext) -> Entity<TaskBoardStore> {
        zlog::init_test();
        cx.update(|cx| {
            TaskBoardStore::init_global(cx);
            TaskBoardStore::global(cx)
        })
    }

    fn register_test_project(
        store: &Entity<TaskBoardStore>,
        cx: &mut TestAppContext,
    ) -> BoardProjectId {
        store.update(cx, |store, cx| {
            store.register_project(
                PathList::new(&["/tmp/repo"]),
                None,
                "repo".into(),
                cx,
            )
        })
    }

    fn column_titles(
        store: &Entity<TaskBoardStore>,
        status: TaskStatus,
        cx: &mut TestAppContext,
    ) -> Vec<String> {
        store.update(cx, |store, _| {
            store
                .tasks_for_column(status, &BoardFilter::default())
                .iter()
                .map(|task| task.title.to_string())
                .collect()
        })
    }

    #[gpui::test]
    async fn test_create_and_order_tasks(cx: &mut TestAppContext) {
        let store = init_test(cx);
        store.read_with(cx, |store, _| store.reload_task()).await;
        let project = register_test_project(&store, cx);

        for title in ["one", "two", "three"] {
            store.update(cx, |store, cx| {
                store.create_task(project, title.into(), None, TaskStatus::Todo, cx)
            });
        }
        assert_eq!(
            column_titles(&store, TaskStatus::Todo, cx),
            ["one", "two", "three"]
        );

        // Move "three" to the top of the column.
        let three = store.update(cx, |store, _| {
            store
                .tasks_for_column(TaskStatus::Todo, &BoardFilter::default())
                .iter()
                .find(|task| task.title.as_ref() == "three")
                .map(|task| task.task_id)
                .expect("task should exist")
        });
        store.update(cx, |store, cx| {
            store.move_task(three, TaskStatus::Todo, 0, cx)
        });
        assert_eq!(
            column_titles(&store, TaskStatus::Todo, cx),
            ["three", "one", "two"]
        );

        // Move "three" to another column.
        store.update(cx, |store, cx| {
            store.move_task(three, TaskStatus::InProgress, 0, cx)
        });
        assert_eq!(column_titles(&store, TaskStatus::Todo, cx), ["one", "two"]);
        assert_eq!(
            column_titles(&store, TaskStatus::InProgress, cx),
            ["three"]
        );
    }

    #[gpui::test]
    async fn test_sort_order_renormalization(cx: &mut TestAppContext) {
        let store = init_test(cx);
        store.read_with(cx, |store, _| store.reload_task()).await;
        let project = register_test_project(&store, cx);

        for title in ["a", "b", "c"] {
            store.update(cx, |store, cx| {
                store.create_task(project, title.into(), None, TaskStatus::Todo, cx)
            });
        }

        // Repeatedly move the column's first task to index 1. Each move lands
        // between the two remaining tasks, halving the sort-order gap until
        // the store must renormalize the column.
        let mut renormalized = 0;
        for iteration in 0..80 {
            let mover = store.update(cx, |store, cx| {
                let id = store
                    .tasks_for_column(TaskStatus::Todo, &BoardFilter::default())
                    .first()
                    .map(|task| task.task_id)
                    .expect("column should not be empty");
                store.move_task(id, TaskStatus::Todo, 1, cx);
                id
            });
            let orders: Vec<f64> = store.update(cx, |store, _| {
                store
                    .tasks_for_column(TaskStatus::Todo, &BoardFilter::default())
                    .iter()
                    .map(|task| task.sort_order)
                    .collect()
            });
            assert_eq!(orders.len(), 3);
            assert!(
                orders.windows(2).all(|pair| pair[0] < pair[1]),
                "orders must stay strictly increasing (iteration {iteration}, moved {mover:?}): {orders:?}"
            );
            // A renormalized column has whole-number orders; midpoint moves
            // always produce a fractional order for the moved task.
            if orders.iter().all(|order| order.fract() == 0.0) {
                renormalized += 1;
            }
        }
        // 80 halvings of the initial gap crosses the renormalization
        // threshold multiple times.
        assert!(
            renormalized > 0,
            "the renormalization path should have run at least once"
        );
    }

    #[gpui::test]
    async fn test_filters(cx: &mut TestAppContext) {
        let store = init_test(cx);
        store.read_with(cx, |store, _| store.reload_task()).await;
        let project = register_test_project(&store, cx);

        let tagged = store.update(cx, |store, cx| {
            store.create_task(project, "tagged task".into(), None, TaskStatus::Todo, cx)
        });
        let archived = store.update(cx, |store, cx| {
            store.create_task(project, "archived task".into(), None, TaskStatus::Todo, cx)
        });
        store.update(cx, |store, cx| {
            store.update_task(tagged, |task| task.tags = vec!["backend".into()], cx);
            store.set_task_archived(archived, true, cx);
        });

        let default_filter = BoardFilter::default();
        let tag_filter = BoardFilter {
            tags: vec!["backend".into()],
            ..Default::default()
        };
        let query_filter = BoardFilter {
            query: "ARCHIVED".into(),
            show_archived: true,
            ..Default::default()
        };

        store.update(cx, |store, _| {
            assert_eq!(
                store.tasks_for_column(TaskStatus::Todo, &default_filter).len(),
                1,
                "archived task should be hidden by default"
            );
            assert_eq!(
                store
                    .tasks_for_column(TaskStatus::Todo, &tag_filter)
                    .first()
                    .map(|task| task.task_id),
                Some(tagged)
            );
            assert_eq!(
                store
                    .tasks_for_column(TaskStatus::Todo, &query_filter)
                    .first()
                    .map(|task| task.task_id),
                Some(archived),
                "query should match case-insensitively"
            );
        });
    }

    #[gpui::test]
    async fn test_non_git_project_detection(cx: &mut TestAppContext) {
        let store = init_test(cx);
        store.read_with(cx, |store, _| store.reload_task()).await;

        let temp = tempfile::tempdir().expect("create temp dir");
        let git_dir = temp.path().join("with-git");
        std::fs::create_dir_all(git_dir.join(".git")).expect("create git project");
        let nested_dir = git_dir.join("nested");
        std::fs::create_dir_all(&nested_dir).expect("create nested folder");
        let plain_dir = temp.path().join("plain");
        std::fs::create_dir_all(&plain_dir).expect("create plain project");

        let git_project = store.update(cx, |store, cx| {
            store.register_project(PathList::new(&[git_dir]), None, "with-git".into(), cx)
        });
        // A folder nested inside a repository counts as git too.
        let nested_project = store.update(cx, |store, cx| {
            store.register_project(PathList::new(&[nested_dir]), None, "nested".into(), cx)
        });
        let plain_project = store.update(cx, |store, cx| {
            store.register_project(PathList::new(&[plain_dir]), None, "plain".into(), cx)
        });
        cx.run_until_parked();

        store.read_with(cx, |store, _| {
            assert!(!store.project_is_non_git(git_project));
            assert!(!store.project_is_non_git(nested_project));
            assert!(
                store.project_is_non_git(plain_project),
                "a project without a .git anywhere above it must be flagged"
            );
        });
    }

    #[gpui::test]
    async fn test_project_registration_dedupes(cx: &mut TestAppContext) {
        let store = init_test(cx);
        store.read_with(cx, |store, _| store.reload_task()).await;

        let first = register_test_project(&store, cx);
        let second = register_test_project(&store, cx);
        assert_eq!(first, second);
        assert_eq!(store.update(cx, |store, _| store.projects.len()), 1);
    }

    #[gpui::test]
    async fn test_session_records(cx: &mut TestAppContext) {
        let store = init_test(cx);
        store.read_with(cx, |store, _| store.reload_task()).await;
        let project = register_test_project(&store, cx);
        let task = store.update(cx, |store, cx| {
            store.create_task(project, "with sessions".into(), None, TaskStatus::InProgress, cx)
        });

        let session = TaskSession {
            session_id: TaskSessionId::new(),
            task_id: task,
            terminal_id: Some("terminal-1".into()),
            agent: "claude".into(),
            label: None,
            working_directory: Some("/tmp/worktree".into()),
            resume_command: Some("claude --continue".into()),
            archived: false,
            created_at: Utc::now(),
            archived_at: None,
        };
        let session_id = session.session_id;
        store.update(cx, |store, cx| {
            store.upsert_session(session, cx);
            store.set_session_runtime(session_id, SessionRuntimeState::NeedsAttention, cx);
        });

        store.update(cx, |store, _| {
            assert!(store.task_needs_attention(task));
            assert_eq!(
                store
                    .session_for_terminal("terminal-1")
                    .map(|session| session.session_id),
                Some(session_id)
            );
        });

        store.update(cx, |store, cx| store.archive_session(session_id, cx));
        store.update(cx, |store, _| {
            assert!(!store.task_needs_attention(task));
            assert!(store.session_for_terminal("terminal-1").is_none());
            let infos = store.sessions_for_task(task);
            assert_eq!(infos.len(), 1);
            assert!(infos[0].session.archived);
            assert_eq!(infos[0].runtime, SessionRuntimeState::Dormant);
            assert_eq!(
                infos[0].session.resume_command.as_deref(),
                Some("claude --continue")
            );
        });
    }

    #[gpui::test]
    async fn test_archived_worktree_records(cx: &mut TestAppContext) {
        let store = init_test(cx);
        store.read_with(cx, |store, _| store.reload_task()).await;
        let project = register_test_project(&store, cx);
        let task = store.update(cx, |store, cx| {
            store.create_task(project, "finished".into(), None, TaskStatus::Done, cx)
        });

        let row = TaskBoardArchivedWorktree {
            task_id: task,
            worktree_path: "/tmp/worktrees/finished/repo".into(),
            main_repo_path: "/tmp/repo".into(),
            branch_name: Some("task/finished".into()),
            staged_commit_hash: "aaa".into(),
            unstaged_commit_hash: "bbb".into(),
            original_commit_hash: "ccc".into(),
            ref_name: "refs/task-board-archived/x-finished".into(),
        };
        store.update(cx, |store, cx| store.save_archived_worktree(row.clone(), cx));
        cx.run_until_parked();

        // A second store over the same database sees the archived worktree.
        let db = store.read_with(cx, |store, _| store.db.clone());
        let reloaded = cx.update(|cx| cx.new(|cx| TaskBoardStore::new(db, cx)));
        reloaded.read_with(cx, |store, _| store.reload_task()).await;
        reloaded.update(cx, |store, cx| {
            let rows: Vec<_> = store.archived_worktrees_for_task(task).cloned().collect();
            assert_eq!(rows, vec![row]);
            store.clear_archived_worktrees(task, cx);
            assert_eq!(store.archived_worktrees_for_task(task).count(), 0);
        });
    }

    #[gpui::test]
    async fn test_persistence_round_trip(cx: &mut TestAppContext) {
        let store = init_test(cx);
        store.read_with(cx, |store, _| store.reload_task()).await;

        let project = register_test_project(&store, cx);
        let task = store.update(cx, |store, cx| {
            store.create_task(project, "persisted".into(), None, TaskStatus::Blocked, cx)
        });
        store.update(cx, |store, cx| {
            store.update_task(task, |task| task.tags = vec!["infra".into()], cx);
        });
        // Let the queued database operations drain.
        cx.run_until_parked();

        // A second store over the same database sees the same state.
        let db = store.read_with(cx, |store, _| store.db.clone());
        let reloaded = cx.update(|cx| cx.new(|cx| TaskBoardStore::new(db, cx)));
        reloaded.read_with(cx, |store, _| store.reload_task()).await;
        reloaded.update(cx, |store, _| {
            let task = store.task(task).expect("task should be persisted");
            assert_eq!(task.title.as_ref(), "persisted");
            assert_eq!(task.status, TaskStatus::Blocked);
            assert_eq!(task.tags, vec![SharedString::from("infra")]);
            assert_eq!(store.projects.len(), 1);
        });
    }

    #[gpui::test]
    async fn test_multi_project_round_trip(cx: &mut TestAppContext) {
        let store = init_test(cx);
        store.read_with(cx, |store, _| store.reload_task()).await;

        let primary = register_test_project(&store, cx);
        let sdk = store.update(cx, |store, cx| {
            store.register_project(PathList::new(&["/tmp/sdk"]), None, "sdk".into(), cx)
        });

        let task = store.update(cx, |store, cx| {
            store.create_task(primary, "spans repos".into(), None, TaskStatus::Todo, cx)
        });
        store.update(cx, |store, cx| {
            store.update_task(
                task,
                |task| {
                    // The primary and a duplicate must be filtered out.
                    task.extra_project_ids = vec![sdk, primary, sdk];
                    task.worktree_paths =
                        PathList::new(&["/tmp/worktrees/spans/repo", "/tmp/worktrees/spans/sdk"]);
                },
                cx,
            );
        });
        cx.run_until_parked();

        let db = store.read_with(cx, |store, _| store.db.clone());
        let reloaded = cx.update(|cx| cx.new(|cx| TaskBoardStore::new(db, cx)));
        reloaded.read_with(cx, |store, _| store.reload_task()).await;
        reloaded.update(cx, |store, _| {
            let task = store.task(task).expect("task should be persisted");
            assert_eq!(task.extra_project_ids, vec![sdk]);
            assert_eq!(
                task.worktree_paths.ordered_paths().collect::<Vec<_>>(),
                [
                    &std::path::PathBuf::from("/tmp/worktrees/spans/repo"),
                    &std::path::PathBuf::from("/tmp/worktrees/spans/sdk"),
                ]
            );
            assert_eq!(
                task.worktree_path(),
                Some(std::path::PathBuf::from("/tmp/worktrees/spans/repo"))
            );

            // Filtering by either project surfaces the task.
            for project in [primary, sdk] {
                let filter = BoardFilter {
                    projects: Some(vec![project]),
                    ..Default::default()
                };
                assert_eq!(store.tasks_for_column(TaskStatus::Todo, &filter).len(), 1);
            }
        });
    }
}
