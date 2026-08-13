use collections::HashMap;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings_macros::{MergeFrom, with_fallible_options};

#[with_fallible_options]
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct TaskBoardSettingsContent {
    /// Agent CLIs available for task board sessions, keyed by name.
    pub agents: Option<HashMap<String, TaskBoardAgentContent>>,
    /// The agent used by default for new task board sessions.
    ///
    /// Default: claude
    pub default_agent: Option<String>,
    /// Prefix for branch names generated when starting a task.
    ///
    /// Default: "task/"
    pub branch_prefix: Option<String>,
    /// Task statuses hidden on the board.
    ///
    /// Default: []
    pub hidden_statuses: Option<Vec<String>>,
    /// Whether to automatically archive a task's worktree (saving
    /// uncommitted changes restorably) when the task is marked done or
    /// cancelled.
    ///
    /// Default: true
    pub cleanup_worktree_on_done: Option<bool>,
}

#[with_fallible_options]
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct TaskBoardAgentContent {
    /// Command sent to the session's shell to start the agent.
    pub command: Option<String>,
    /// Command used instead of `command` when restoring an archived session.
    pub resume_command: Option<String>,
}
