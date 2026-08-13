use std::path::PathBuf;

use anyhow::Context as _;
use chrono::{DateTime, Utc};
use db::{
    sqlez::{
        bindable::{Bind, Column},
        domain::Domain,
        statement::Statement,
        thread_safe_connection::ThreadSafeConnection,
    },
    sqlez_macros::sql,
};
use gpui::SharedString;
use project::ProjectGroupKey;
use remote::RemoteConnectionOptions;
use util::path_list::{PathList, SerializedPathList};

macro_rules! board_id_type {
    ($name:ident) => {
        #[derive(
            Clone, Copy, PartialEq, Eq, Hash, Debug, serde::Serialize, serde::Deserialize,
        )]
        pub struct $name(uuid::Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(uuid::Uuid::new_v4())
            }

            /// Stable, hyphenated string form suitable for use as a key.
            pub fn to_key_string(&self) -> String {
                self.0.hyphenated().to_string()
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl Bind for $name {
            fn bind(&self, statement: &Statement, start_index: i32) -> anyhow::Result<i32> {
                self.0.bind(statement, start_index)
            }
        }

        impl Column for $name {
            fn column(statement: &mut Statement, start_index: i32) -> anyhow::Result<(Self, i32)> {
                let (uuid, next) = Column::column(statement, start_index)?;
                Ok(($name(uuid), next))
            }
        }
    };
}

board_id_type!(BoardProjectId);
board_id_type!(BoardTaskId);
board_id_type!(TaskSessionId);

#[derive(
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Debug,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Backlog,
    Todo,
    InProgress,
    InReview,
    Blocked,
    Done,
    Cancelled,
}

impl TaskStatus {
    pub const ALL: [TaskStatus; 7] = [
        TaskStatus::Backlog,
        TaskStatus::Todo,
        TaskStatus::InProgress,
        TaskStatus::InReview,
        TaskStatus::Blocked,
        TaskStatus::Done,
        TaskStatus::Cancelled,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            TaskStatus::Backlog => "backlog",
            TaskStatus::Todo => "todo",
            TaskStatus::InProgress => "in_progress",
            TaskStatus::InReview => "in_review",
            TaskStatus::Blocked => "blocked",
            TaskStatus::Done => "done",
            TaskStatus::Cancelled => "cancelled",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|status| status.as_str() == text)
    }

    pub fn label(&self) -> &'static str {
        match self {
            TaskStatus::Backlog => "Backlog",
            TaskStatus::Todo => "Todo",
            TaskStatus::InProgress => "In Progress",
            TaskStatus::InReview => "In Review",
            TaskStatus::Blocked => "Blocked",
            TaskStatus::Done => "Done",
            TaskStatus::Cancelled => "Cancelled",
        }
    }
}

impl Bind for TaskStatus {
    fn bind(&self, statement: &Statement, start_index: i32) -> anyhow::Result<i32> {
        self.as_str().bind(statement, start_index)
    }
}

impl Column for TaskStatus {
    fn column(statement: &mut Statement, start_index: i32) -> anyhow::Result<(Self, i32)> {
        let (text, next): (String, i32) = Column::column(statement, start_index)?;
        let status = Self::parse(&text)
            .with_context(|| format!("unknown task status in database: {text}"))?;
        Ok((status, next))
    }
}

/// A repository registered on the task board. Identity mirrors
/// `ProjectGroupKey`: the main git worktree paths plus the remote host.
#[derive(Debug, Clone, PartialEq)]
pub struct BoardProject {
    pub project_id: BoardProjectId,
    pub main_worktree_paths: PathList,
    pub remote_connection: Option<RemoteConnectionOptions>,
    pub display_name: SharedString,
    pub created_at: DateTime<Utc>,
}

impl BoardProject {
    pub fn group_key(&self) -> ProjectGroupKey {
        ProjectGroupKey::new(
            self.remote_connection.clone(),
            self.main_worktree_paths.clone(),
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoardTask {
    pub task_id: BoardTaskId,
    /// The task's primary project; the PR helper and session working
    /// directories default to it.
    pub project_id: BoardProjectId,
    /// Additional projects the task spans; starting the task creates a
    /// worktree in each of them alongside the primary's.
    pub extra_project_ids: Vec<BoardProjectId>,
    pub title: SharedString,
    pub description: Option<String>,
    pub status: TaskStatus,
    pub sort_order: f64,
    pub branch_name: Option<String>,
    /// JSON-serialized `zed_actions::NewWorktreeBranchTarget` chosen as the
    /// base when starting this task.
    pub base_branch_target: Option<String>,
    /// One worktree per repository the task spans; the first is the
    /// primary project's.
    pub worktree_paths: PathList,
    pub pr_url: Option<String>,
    pub archived: bool,
    pub tags: Vec<SharedString>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl BoardTask {
    pub fn worktree_path(&self) -> Option<PathBuf> {
        self.worktree_paths.ordered_paths().next().cloned()
    }

    pub fn has_worktree(&self) -> bool {
        !self.worktree_paths.is_empty()
    }

    /// True when the task runs directly in its project's folders because the
    /// project has no git repository. Started tasks in git projects always
    /// get a branch alongside their worktree, so a branchless task with
    /// worktree paths is in-place: there is no task-created worktree to
    /// archive or remove.
    pub fn runs_in_place(&self) -> bool {
        self.has_worktree() && self.branch_name.is_none()
    }

    pub fn all_project_ids(&self) -> impl Iterator<Item = BoardProjectId> + '_ {
        std::iter::once(self.project_id).chain(self.extra_project_ids.iter().copied())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TaskSession {
    pub session_id: TaskSessionId,
    pub task_id: BoardTaskId,
    /// Key string of the `agent_ui` terminal thread backing this session
    /// while it is live. Cleared when the session is archived.
    pub terminal_id: Option<String>,
    /// Key into the `task_board.agents` setting.
    pub agent: String,
    pub label: Option<SharedString>,
    pub working_directory: Option<PathBuf>,
    /// Concrete command typed into a fresh shell when restoring this session.
    pub resume_command: Option<String>,
    pub archived: bool,
    pub created_at: DateTime<Utc>,
    pub archived_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PrState {
    Open,
    Draft,
    Merged,
    Closed,
    /// The PR was recorded but its status has not been fetched (e.g. the
    /// GitHub CLI is unavailable or the URL predates status tracking).
    Unknown,
}

impl PrState {
    pub fn as_str(&self) -> &'static str {
        match self {
            PrState::Open => "open",
            PrState::Draft => "draft",
            PrState::Merged => "merged",
            PrState::Closed => "closed",
            PrState::Unknown => "unknown",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        [
            PrState::Open,
            PrState::Draft,
            PrState::Merged,
            PrState::Closed,
            PrState::Unknown,
        ]
        .into_iter()
        .find(|state| state.as_str() == text)
    }

    pub fn label(&self) -> &'static str {
        match self {
            PrState::Open => "Open",
            PrState::Draft => "Draft",
            PrState::Merged => "Merged",
            PrState::Closed => "Closed",
            PrState::Unknown => "Status unknown",
        }
    }
}

/// A pull request associated with a task's branch, discovered via the
/// GitHub CLI (or seeded from a stored PR URL with unknown status).
#[derive(Debug, Clone, PartialEq)]
pub struct TaskPullRequest {
    pub task_id: BoardTaskId,
    pub url: String,
    pub number: Option<i64>,
    pub title: Option<String>,
    pub state: PrState,
    pub updated_at: Option<DateTime<Utc>>,
    /// Detached PRs stay recorded (so branch discovery doesn't re-attach
    /// them) but are hidden from the card and dimmed in the detail view.
    pub detached: bool,
}

impl Column for TaskPullRequest {
    fn column(statement: &mut Statement, start_index: i32) -> anyhow::Result<(Self, i32)> {
        let (task_id, next) = BoardTaskId::column(statement, start_index)?;
        let (url, next): (String, i32) = Column::column(statement, next)?;
        let (number, next): (Option<i64>, i32) = Column::column(statement, next)?;
        let (title, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (state, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (updated_at, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (detached, next): (Option<bool>, i32) = Column::column(statement, next)?;

        Ok((
            TaskPullRequest {
                task_id,
                url,
                number,
                title,
                state: state
                    .as_deref()
                    .and_then(PrState::parse)
                    .unwrap_or(PrState::Unknown),
                updated_at: updated_at.as_deref().map(parse_timestamp).transpose()?,
                detached: detached.unwrap_or(false),
            },
            next,
        ))
    }
}

/// Git state saved when a task's worktree is cleaned up, sufficient to
/// recreate the worktree with its uncommitted changes intact.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskBoardArchivedWorktree {
    pub task_id: BoardTaskId,
    pub worktree_path: PathBuf,
    pub main_repo_path: PathBuf,
    pub branch_name: Option<String>,
    pub staged_commit_hash: String,
    pub unstaged_commit_hash: String,
    pub original_commit_hash: String,
    /// Ref protecting the checkpoint commits from GC, in the
    /// `refs/task-board-archived/` namespace.
    pub ref_name: String,
}

fn parse_timestamp(text: &str) -> anyhow::Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(text)
        .with_context(|| format!("invalid timestamp in task board database: {text}"))?
        .with_timezone(&Utc))
}

fn serialize_path_list(paths: &PathList) -> (String, String) {
    let serialized = paths.serialize();
    (serialized.paths, serialized.order)
}

fn path_from_string(text: Option<String>) -> Option<PathBuf> {
    text.map(PathBuf::from)
}

impl Column for BoardProject {
    fn column(statement: &mut Statement, start_index: i32) -> anyhow::Result<(Self, i32)> {
        let (project_id, next) = BoardProjectId::column(statement, start_index)?;
        let (paths, next): (String, i32) = Column::column(statement, next)?;
        let (order, next): (String, i32) = Column::column(statement, next)?;
        let (remote_connection, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (display_name, next): (String, i32) = Column::column(statement, next)?;
        let (created_at, next): (String, i32) = Column::column(statement, next)?;

        let remote_connection = remote_connection
            .map(|json| serde_json::from_str::<RemoteConnectionOptions>(&json))
            .transpose()
            .context("deserialize board project remote connection")?;

        Ok((
            BoardProject {
                project_id,
                main_worktree_paths: PathList::deserialize(&SerializedPathList { paths, order }),
                remote_connection,
                display_name: display_name.into(),
                created_at: parse_timestamp(&created_at)?,
            },
            next,
        ))
    }
}

impl Column for BoardTask {
    fn column(statement: &mut Statement, start_index: i32) -> anyhow::Result<(Self, i32)> {
        let (task_id, next) = BoardTaskId::column(statement, start_index)?;
        let (project_id, next) = BoardProjectId::column(statement, next)?;
        let (title, next): (String, i32) = Column::column(statement, next)?;
        let (description, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (status, next) = TaskStatus::column(statement, next)?;
        let (sort_order, next): (f64, i32) = Column::column(statement, next)?;
        let (branch_name, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (base_branch_target, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (worktree_path, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (worktree_paths, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (worktree_paths_order, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (pr_url, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (archived, next): (bool, i32) = Column::column(statement, next)?;
        let (created_at, next): (String, i32) = Column::column(statement, next)?;
        let (updated_at, next): (String, i32) = Column::column(statement, next)?;

        // Rows written before multi-project support only have the single
        // worktree_path column. That column stays authoritative for
        // emptiness: a pre-multi-project build finishing a task clears only
        // it, and trusting a stale worktree_paths would resurrect
        // already-archived worktrees after an upgrade.
        let worktree_paths = match (path_from_string(worktree_path), worktree_paths) {
            (None, _) => PathList::new::<PathBuf>(&[]),
            (Some(_), Some(paths)) if !paths.is_empty() => {
                PathList::deserialize(&SerializedPathList {
                    paths,
                    order: worktree_paths_order.unwrap_or_default(),
                })
            }
            (Some(path), _) => PathList::new(&[path]),
        };

        Ok((
            BoardTask {
                task_id,
                project_id,
                extra_project_ids: Vec::new(),
                title: title.into(),
                description,
                status,
                sort_order,
                branch_name,
                base_branch_target,
                worktree_paths,
                pr_url,
                archived,
                tags: Vec::new(),
                created_at: parse_timestamp(&created_at)?,
                updated_at: parse_timestamp(&updated_at)?,
            },
            next,
        ))
    }
}

impl Column for TaskSession {
    fn column(statement: &mut Statement, start_index: i32) -> anyhow::Result<(Self, i32)> {
        let (session_id, next) = TaskSessionId::column(statement, start_index)?;
        let (task_id, next) = BoardTaskId::column(statement, next)?;
        let (terminal_id, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (agent, next): (String, i32) = Column::column(statement, next)?;
        let (label, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (working_directory, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (resume_command, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (archived, next): (bool, i32) = Column::column(statement, next)?;
        let (created_at, next): (String, i32) = Column::column(statement, next)?;
        let (archived_at, next): (Option<String>, i32) = Column::column(statement, next)?;

        Ok((
            TaskSession {
                session_id,
                task_id,
                terminal_id,
                agent,
                label: label.map(SharedString::from),
                working_directory: path_from_string(working_directory),
                resume_command,
                archived,
                created_at: parse_timestamp(&created_at)?,
                archived_at: archived_at.as_deref().map(parse_timestamp).transpose()?,
            },
            next,
        ))
    }
}

impl Column for TaskBoardArchivedWorktree {
    fn column(statement: &mut Statement, start_index: i32) -> anyhow::Result<(Self, i32)> {
        let (task_id, next) = BoardTaskId::column(statement, start_index)?;
        let (worktree_path, next): (String, i32) = Column::column(statement, next)?;
        let (main_repo_path, next): (String, i32) = Column::column(statement, next)?;
        let (branch_name, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (staged_commit_hash, next): (String, i32) = Column::column(statement, next)?;
        let (unstaged_commit_hash, next): (String, i32) = Column::column(statement, next)?;
        let (original_commit_hash, next): (String, i32) = Column::column(statement, next)?;
        let (ref_name, next): (String, i32) = Column::column(statement, next)?;

        Ok((
            TaskBoardArchivedWorktree {
                task_id,
                worktree_path: PathBuf::from(worktree_path),
                main_repo_path: PathBuf::from(main_repo_path),
                branch_name,
                staged_commit_hash,
                unstaged_commit_hash,
                original_commit_hash,
                ref_name,
            },
            next,
        ))
    }
}

pub(crate) struct TaskBoardDb(pub(crate) ThreadSafeConnection);

impl Domain for TaskBoardDb {
    const NAME: &str = stringify!(TaskBoardDb);

    const MIGRATIONS: &[&str] = &[sql!(
        CREATE TABLE IF NOT EXISTS board_projects(
            project_id BLOB PRIMARY KEY,
            main_worktree_paths TEXT NOT NULL,
            main_worktree_paths_order TEXT NOT NULL,
            remote_connection TEXT,
            display_name TEXT NOT NULL,
            created_at TEXT NOT NULL
        ) STRICT;

        CREATE TABLE IF NOT EXISTS board_tasks(
            task_id BLOB PRIMARY KEY,
            project_id BLOB NOT NULL,
            title TEXT NOT NULL,
            description TEXT,
            status TEXT NOT NULL,
            sort_order REAL NOT NULL,
            branch_name TEXT,
            base_branch_target TEXT,
            worktree_path TEXT,
            pr_url TEXT,
            archived INTEGER NOT NULL DEFAULT 0,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        ) STRICT;

        CREATE TABLE IF NOT EXISTS board_task_tags(
            task_id BLOB NOT NULL,
            tag TEXT NOT NULL,
            PRIMARY KEY(task_id, tag)
        ) STRICT;

        CREATE TABLE IF NOT EXISTS board_sessions(
            session_id BLOB PRIMARY KEY,
            task_id BLOB NOT NULL,
            terminal_id TEXT,
            agent TEXT NOT NULL,
            label TEXT,
            working_directory TEXT,
            resume_command TEXT,
            archived INTEGER NOT NULL DEFAULT 0,
            created_at TEXT NOT NULL,
            archived_at TEXT
        ) STRICT;

        CREATE TABLE IF NOT EXISTS board_task_archived_worktrees(
            task_id BLOB NOT NULL,
            worktree_path TEXT NOT NULL,
            main_repo_path TEXT NOT NULL,
            branch_name TEXT,
            staged_commit_hash TEXT NOT NULL,
            unstaged_commit_hash TEXT NOT NULL,
            original_commit_hash TEXT NOT NULL,
            ref_name TEXT NOT NULL,
            PRIMARY KEY(task_id, worktree_path)
        ) STRICT;
    ),
    sql!(
        CREATE TABLE IF NOT EXISTS board_task_prs(
            task_id BLOB NOT NULL,
            url TEXT NOT NULL,
            number INTEGER,
            title TEXT,
            state TEXT,
            updated_at TEXT,
            PRIMARY KEY(task_id, url)
        ) STRICT;

        INSERT OR IGNORE INTO board_task_prs(task_id, url)
        SELECT task_id, pr_url FROM board_tasks WHERE pr_url IS NOT NULL;
    ),
    sql!(
        ALTER TABLE board_task_prs ADD COLUMN detached INTEGER DEFAULT 0;
    ),
    sql!(
        CREATE TABLE IF NOT EXISTS board_task_projects(
            task_id BLOB NOT NULL,
            project_id BLOB NOT NULL,
            PRIMARY KEY(task_id, project_id)
        ) STRICT;

        ALTER TABLE board_tasks ADD COLUMN worktree_paths TEXT;
        ALTER TABLE board_tasks ADD COLUMN worktree_paths_order TEXT;
    )];
}

db::static_connection!(TaskBoardDb, []);

impl TaskBoardDb {
    pub fn list_projects(&self) -> anyhow::Result<Vec<BoardProject>> {
        self.select::<BoardProject>(
            "SELECT project_id, main_worktree_paths, main_worktree_paths_order, \
            remote_connection, display_name, created_at \
            FROM board_projects \
            ORDER BY created_at ASC",
        )?()
    }

    pub fn list_tasks(&self) -> anyhow::Result<Vec<BoardTask>> {
        self.select::<BoardTask>(
            "SELECT task_id, project_id, title, description, status, sort_order, \
            branch_name, base_branch_target, worktree_path, worktree_paths, \
            worktree_paths_order, pr_url, archived, created_at, updated_at \
            FROM board_tasks \
            ORDER BY sort_order ASC",
        )?()
    }

    pub fn list_task_projects(&self) -> anyhow::Result<Vec<(BoardTaskId, BoardProjectId)>> {
        self.select::<(BoardTaskId, BoardProjectId)>(
            "SELECT task_id, project_id FROM board_task_projects",
        )?()
    }

    pub fn list_tags(&self) -> anyhow::Result<Vec<(BoardTaskId, String)>> {
        self.select::<(BoardTaskId, String)>(
            "SELECT task_id, tag FROM board_task_tags ORDER BY tag ASC",
        )?()
    }

    pub fn list_sessions(&self) -> anyhow::Result<Vec<TaskSession>> {
        self.select::<TaskSession>(
            "SELECT session_id, task_id, terminal_id, agent, label, working_directory, \
            resume_command, archived, created_at, archived_at \
            FROM board_sessions \
            ORDER BY created_at ASC",
        )?()
    }

    pub fn list_archived_worktrees(&self) -> anyhow::Result<Vec<TaskBoardArchivedWorktree>> {
        self.select::<TaskBoardArchivedWorktree>(
            "SELECT task_id, worktree_path, main_repo_path, branch_name, \
            staged_commit_hash, unstaged_commit_hash, original_commit_hash, ref_name \
            FROM board_task_archived_worktrees",
        )?()
    }

    pub async fn save_project(&self, project: BoardProject) -> anyhow::Result<()> {
        let (paths, order) = serialize_path_list(&project.main_worktree_paths);
        let remote_connection = project
            .remote_connection
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .context("serialize board project remote connection")?;
        let display_name = project.display_name.to_string();
        let created_at = project.created_at.to_rfc3339();

        self.write(move |conn| {
            let sql = "INSERT INTO board_projects(project_id, main_worktree_paths, \
                main_worktree_paths_order, remote_connection, display_name, created_at) \
                VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                ON CONFLICT(project_id) DO UPDATE SET \
                    main_worktree_paths = excluded.main_worktree_paths, \
                    main_worktree_paths_order = excluded.main_worktree_paths_order, \
                    remote_connection = excluded.remote_connection, \
                    display_name = excluded.display_name";
            let mut statement = Statement::prepare(conn, sql)?;
            let mut index = statement.bind(&project.project_id, 1)?;
            index = statement.bind(&paths, index)?;
            index = statement.bind(&order, index)?;
            index = statement.bind(&remote_connection, index)?;
            index = statement.bind(&display_name, index)?;
            statement.bind(&created_at, index)?;
            statement.exec()
        })
        .await
    }

    pub async fn save_task(&self, task: BoardTask) -> anyhow::Result<()> {
        let title = task.title.to_string();
        // The single-path column stays populated with the primary worktree so
        // downgrading to an older build keeps working.
        let worktree_path = task
            .worktree_path()
            .map(|path| path.to_string_lossy().into_owned());
        let (worktree_paths, worktree_paths_order) = serialize_path_list(&task.worktree_paths);
        let created_at = task.created_at.to_rfc3339();
        let updated_at = task.updated_at.to_rfc3339();

        self.write(move |conn| {
            let sql = "INSERT INTO board_tasks(task_id, project_id, title, description, \
                status, sort_order, branch_name, base_branch_target, worktree_path, \
                worktree_paths, worktree_paths_order, pr_url, \
                archived, created_at, updated_at) \
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15) \
                ON CONFLICT(task_id) DO UPDATE SET \
                    project_id = excluded.project_id, \
                    title = excluded.title, \
                    description = excluded.description, \
                    status = excluded.status, \
                    sort_order = excluded.sort_order, \
                    branch_name = excluded.branch_name, \
                    base_branch_target = excluded.base_branch_target, \
                    worktree_path = excluded.worktree_path, \
                    worktree_paths = excluded.worktree_paths, \
                    worktree_paths_order = excluded.worktree_paths_order, \
                    pr_url = excluded.pr_url, \
                    archived = excluded.archived, \
                    updated_at = excluded.updated_at";
            let mut statement = Statement::prepare(conn, sql)?;
            let mut index = statement.bind(&task.task_id, 1)?;
            index = statement.bind(&task.project_id, index)?;
            index = statement.bind(&title, index)?;
            index = statement.bind(&task.description, index)?;
            index = statement.bind(&task.status, index)?;
            index = statement.bind(&task.sort_order, index)?;
            index = statement.bind(&task.branch_name, index)?;
            index = statement.bind(&task.base_branch_target, index)?;
            index = statement.bind(&worktree_path, index)?;
            index = statement.bind(&worktree_paths, index)?;
            index = statement.bind(&worktree_paths_order, index)?;
            index = statement.bind(&task.pr_url, index)?;
            index = statement.bind(&task.archived, index)?;
            index = statement.bind(&created_at, index)?;
            statement.bind(&updated_at, index)?;
            statement.exec()
        })
        .await
    }

    pub async fn replace_task_projects(
        &self,
        task_id: BoardTaskId,
        project_ids: Vec<BoardProjectId>,
    ) -> anyhow::Result<()> {
        self.write(move |conn| {
            let mut statement =
                Statement::prepare(conn, "DELETE FROM board_task_projects WHERE task_id = ?")?;
            statement.bind(&task_id, 1)?;
            statement.exec()?;

            for project_id in project_ids {
                let mut statement = Statement::prepare(
                    conn,
                    "INSERT OR IGNORE INTO board_task_projects(task_id, project_id) \
                    VALUES (?1, ?2)",
                )?;
                let index = statement.bind(&task_id, 1)?;
                statement.bind(&project_id, index)?;
                statement.exec()?;
            }
            Ok(())
        })
        .await
    }

    pub async fn delete_task(&self, task_id: BoardTaskId) -> anyhow::Result<()> {
        self.write(move |conn| {
            for sql in [
                "DELETE FROM board_task_tags WHERE task_id = ?",
                "DELETE FROM board_task_projects WHERE task_id = ?",
                "DELETE FROM board_sessions WHERE task_id = ?",
                "DELETE FROM board_task_archived_worktrees WHERE task_id = ?",
                "DELETE FROM board_task_prs WHERE task_id = ?",
                "DELETE FROM board_tasks WHERE task_id = ?",
            ] {
                let mut statement = Statement::prepare(conn, sql)?;
                statement.bind(&task_id, 1)?;
                statement.exec()?;
            }
            Ok(())
        })
        .await
    }

    pub async fn replace_tags(&self, task_id: BoardTaskId, tags: Vec<String>) -> anyhow::Result<()> {
        self.write(move |conn| {
            let mut statement =
                Statement::prepare(conn, "DELETE FROM board_task_tags WHERE task_id = ?")?;
            statement.bind(&task_id, 1)?;
            statement.exec()?;

            for tag in tags {
                let mut statement = Statement::prepare(
                    conn,
                    "INSERT OR IGNORE INTO board_task_tags(task_id, tag) VALUES (?1, ?2)",
                )?;
                let index = statement.bind(&task_id, 1)?;
                statement.bind(&tag, index)?;
                statement.exec()?;
            }
            Ok(())
        })
        .await
    }

    pub async fn update_sort_orders(
        &self,
        orders: Vec<(BoardTaskId, f64)>,
    ) -> anyhow::Result<()> {
        self.write(move |conn| {
            for (task_id, sort_order) in orders {
                let mut statement = Statement::prepare(
                    conn,
                    "UPDATE board_tasks SET sort_order = ?2 WHERE task_id = ?1",
                )?;
                let index = statement.bind(&task_id, 1)?;
                statement.bind(&sort_order, index)?;
                statement.exec()?;
            }
            Ok(())
        })
        .await
    }

    pub fn list_prs(&self) -> anyhow::Result<Vec<TaskPullRequest>> {
        self.select::<TaskPullRequest>(
            "SELECT task_id, url, number, title, state, updated_at, detached \
            FROM board_task_prs \
            ORDER BY number ASC, url ASC",
        )?()
    }

    pub async fn replace_task_prs(
        &self,
        task_id: BoardTaskId,
        prs: Vec<TaskPullRequest>,
    ) -> anyhow::Result<()> {
        self.write(move |conn| {
            let mut statement =
                Statement::prepare(conn, "DELETE FROM board_task_prs WHERE task_id = ?")?;
            statement.bind(&task_id, 1)?;
            statement.exec()?;

            for pr in prs {
                let state = (pr.state != PrState::Unknown).then(|| pr.state.as_str().to_string());
                let updated_at = pr.updated_at.as_ref().map(DateTime::to_rfc3339);
                let mut statement = Statement::prepare(
                    conn,
                    "INSERT OR REPLACE INTO board_task_prs(task_id, url, number, title, \
                    state, updated_at, detached) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                )?;
                let mut index = statement.bind(&pr.task_id, 1)?;
                index = statement.bind(&pr.url, index)?;
                index = statement.bind(&pr.number, index)?;
                index = statement.bind(&pr.title, index)?;
                index = statement.bind(&state, index)?;
                index = statement.bind(&updated_at, index)?;
                statement.bind(&pr.detached, index)?;
                statement.exec()?;
            }
            Ok(())
        })
        .await
    }

    pub async fn save_session(&self, session: TaskSession) -> anyhow::Result<()> {
        let label = session.label.as_ref().map(ToString::to_string);
        let working_directory = session
            .working_directory
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned());
        let created_at = session.created_at.to_rfc3339();
        let archived_at = session.archived_at.as_ref().map(DateTime::to_rfc3339);

        self.write(move |conn| {
            let sql = "INSERT INTO board_sessions(session_id, task_id, terminal_id, agent, \
                label, working_directory, resume_command, archived, created_at, archived_at) \
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) \
                ON CONFLICT(session_id) DO UPDATE SET \
                    task_id = excluded.task_id, \
                    terminal_id = excluded.terminal_id, \
                    agent = excluded.agent, \
                    label = excluded.label, \
                    working_directory = excluded.working_directory, \
                    resume_command = excluded.resume_command, \
                    archived = excluded.archived, \
                    archived_at = excluded.archived_at";
            let mut statement = Statement::prepare(conn, sql)?;
            let mut index = statement.bind(&session.session_id, 1)?;
            index = statement.bind(&session.task_id, index)?;
            index = statement.bind(&session.terminal_id, index)?;
            index = statement.bind(&session.agent, index)?;
            index = statement.bind(&label, index)?;
            index = statement.bind(&working_directory, index)?;
            index = statement.bind(&session.resume_command, index)?;
            index = statement.bind(&session.archived, index)?;
            index = statement.bind(&created_at, index)?;
            statement.bind(&archived_at, index)?;
            statement.exec()
        })
        .await
    }

    pub async fn delete_session(&self, session_id: TaskSessionId) -> anyhow::Result<()> {
        self.write(move |conn| {
            let mut statement =
                Statement::prepare(conn, "DELETE FROM board_sessions WHERE session_id = ?")?;
            statement.bind(&session_id, 1)?;
            statement.exec()
        })
        .await
    }

    pub async fn save_archived_worktree(
        &self,
        row: TaskBoardArchivedWorktree,
    ) -> anyhow::Result<()> {
        let worktree_path = row.worktree_path.to_string_lossy().into_owned();
        let main_repo_path = row.main_repo_path.to_string_lossy().into_owned();

        self.write(move |conn| {
            let sql = "INSERT INTO board_task_archived_worktrees(task_id, worktree_path, \
                main_repo_path, branch_name, staged_commit_hash, unstaged_commit_hash, \
                original_commit_hash, ref_name) \
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
                ON CONFLICT(task_id, worktree_path) DO UPDATE SET \
                    main_repo_path = excluded.main_repo_path, \
                    branch_name = excluded.branch_name, \
                    staged_commit_hash = excluded.staged_commit_hash, \
                    unstaged_commit_hash = excluded.unstaged_commit_hash, \
                    original_commit_hash = excluded.original_commit_hash, \
                    ref_name = excluded.ref_name";
            let mut statement = Statement::prepare(conn, sql)?;
            let mut index = statement.bind(&row.task_id, 1)?;
            index = statement.bind(&worktree_path, index)?;
            index = statement.bind(&main_repo_path, index)?;
            index = statement.bind(&row.branch_name, index)?;
            index = statement.bind(&row.staged_commit_hash, index)?;
            index = statement.bind(&row.unstaged_commit_hash, index)?;
            index = statement.bind(&row.original_commit_hash, index)?;
            statement.bind(&row.ref_name, index)?;
            statement.exec()
        })
        .await
    }

    pub async fn delete_archived_worktrees(&self, task_id: BoardTaskId) -> anyhow::Result<()> {
        self.write(move |conn| {
            let mut statement = Statement::prepare(
                conn,
                "DELETE FROM board_task_archived_worktrees WHERE task_id = ?",
            )?;
            statement.bind(&task_id, 1)?;
            statement.exec()
        })
        .await
    }
}
