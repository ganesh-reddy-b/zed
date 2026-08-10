use collections::HashMap;
use serde::Deserialize;
use settings::{RegisterSetting, Settings};

use crate::task_db::TaskStatus;

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct TaskBoardAgent {
    pub command: String,
    pub resume_command: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, RegisterSetting)]
pub struct TaskBoardSettings {
    pub agents: HashMap<String, TaskBoardAgent>,
    pub default_agent: String,
    pub branch_prefix: String,
    pub hidden_statuses: Vec<TaskStatus>,
    pub cleanup_worktree_on_done: bool,
}

impl Settings for TaskBoardSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let task_board = content.task_board.clone().unwrap();
        Self {
            agents: task_board
                .agents
                .unwrap_or_default()
                .into_iter()
                .filter_map(|(name, agent)| {
                    Some((
                        name,
                        TaskBoardAgent {
                            command: agent.command?,
                            resume_command: agent.resume_command,
                        },
                    ))
                })
                .collect(),
            default_agent: task_board.default_agent.unwrap(),
            branch_prefix: task_board.branch_prefix.unwrap(),
            hidden_statuses: task_board
                .hidden_statuses
                .unwrap_or_default()
                .iter()
                .filter_map(|status| TaskStatus::parse(status))
                .collect(),
            cleanup_worktree_on_done: task_board.cleanup_worktree_on_done.unwrap(),
        }
    }
}
