mod board_view;
mod card_detail_modal;
mod new_task_modal;
mod pr_sync;
mod session_manager;
mod task_board_settings;
mod task_db;
mod task_lifecycle;
mod task_store;

use gpui::{App, AppContext as _, actions};
use workspace::{
    Toast, Workspace, notifications::NotificationId, register_serializable_item,
    with_active_or_new_workspace,
};

pub use board_view::TaskBoardView;
pub use card_detail_modal::CardDetailModal;
pub use new_task_modal::NewTaskModal;
pub use task_board_settings::{TaskBoardAgent, TaskBoardSettings};
pub use task_db::{
    BoardProject, BoardProjectId, BoardTask, BoardTaskId, PrState, TaskBoardArchivedWorktree,
    TaskPullRequest, TaskSession, TaskSessionId, TaskStatus,
};
pub use task_store::{
    BoardFilter, SessionRuntimeState, TaskBoardStore, TaskBoardStoreEvent, TaskSessionInfo,
};

actions!(
    task_board,
    [
        /// Creates a new task on the task board.
        NewTask,
        /// Registers the current project on the task board.
        AddProject
    ]
);

pub fn init(cx: &mut App) {
    task_store::TaskBoardStore::init_global(cx);

    cx.observe_new(|_: &mut agent_ui::AgentPanel, _window, cx| {
        let panel = cx.entity();
        task_store::TaskBoardStore::global(cx).update(cx, |store, cx| {
            store.monitor_agent_panel(panel, cx);
        });
    })
    .detach();

    cx.on_action(|_: &zed_actions::task_board::Open, cx| {
        with_active_or_new_workspace(cx, |workspace, window, cx| {
            let existing = workspace
                .active_pane()
                .read(cx)
                .items()
                .find_map(|item| item.downcast::<TaskBoardView>());

            if let Some(existing) = existing {
                workspace.activate_item(&existing, true, true, window, cx);
            } else {
                let board = cx.new(|cx| TaskBoardView::new(window, cx));
                workspace.add_item_to_active_pane(Box::new(board), None, true, window, cx);
            }
        });
    });

    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, _: &NewTask, window, cx| {
            NewTaskModal::toggle(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &AddProject, _window, cx| {
            struct AddProjectToast;
            if new_task_modal::register_workspace_project(workspace, cx).is_none() {
                workspace.show_toast(
                    Toast::new(
                        NotificationId::unique::<AddProjectToast>(),
                        "Open a folder before adding it to the task board.",
                    ),
                    cx,
                );
            }
        });
    })
    .detach();

    register_serializable_item::<TaskBoardView>(cx);
}
