use db::kvp::KeyValueStore;
use editor::Editor;
use gpui::{
    App, AppContext as _, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render,
    SharedString, TaskExt as _, Window,
};
use project::ProjectGroupKey;
use ui::{ContextMenu, DropdownMenu, prelude::*};
use util::{ResultExt as _, path_list::PathList};
use workspace::{DismissDecision, ModalView, Workspace};

use crate::{BoardProjectId, TaskBoardStore, TaskStatus};

const NEW_TASK_DRAFT_KEY: &str = "task_board_new_task_draft";

/// What the user had typed into the modal when it was dismissed without
/// creating a task; restored the next time the modal opens.
#[derive(serde::Serialize, serde::Deserialize)]
struct NewTaskDraft {
    title: String,
    description: String,
    status: TaskStatus,
    project_id: Option<BoardProjectId>,
}

/// A human-readable name for a project derived from its root folder names.
pub(crate) fn display_name_for_paths(paths: &PathList) -> SharedString {
    let names: Vec<_> = paths
        .ordered_paths()
        .filter_map(|path| path.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .collect();
    if names.is_empty() {
        "Untitled project".into()
    } else {
        names.join(", ").into()
    }
}

pub struct NewTaskModal {
    store: Entity<TaskBoardStore>,
    title_editor: Entity<Editor>,
    description_editor: Entity<Editor>,
    selected_project: Option<BoardProjectId>,
    status: TaskStatus,
    draft_restored: bool,
    created: bool,
}

impl EventEmitter<DismissEvent> for NewTaskModal {}

impl ModalView for NewTaskModal {
    fn on_before_dismiss(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> DismissDecision {
        // Every dismissal path (Esc, Cancel, clicking away) lands here, so
        // whatever was typed survives as a draft unless the task was created.
        if !self.created {
            self.persist_draft(cx);
        }
        DismissDecision::Dismiss(true)
    }
}

impl Focusable for NewTaskModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.title_editor.focus_handle(cx)
    }
}

impl NewTaskModal {
    pub fn toggle(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
        let store = TaskBoardStore::global(cx);

        // Make the current workspace's repository available on the board so
        // creating a task from any project just works.
        let group_key = ProjectGroupKey::from_project(workspace.project().read(cx), cx);
        let selected_project = if group_key.path_list().is_empty() {
            store
                .read(cx)
                .projects()
                .next()
                .map(|project| project.project_id)
        } else {
            Some(store.update(cx, |store, cx| {
                store.register_project(
                    group_key.path_list().clone(),
                    group_key.host(),
                    display_name_for_paths(group_key.path_list()),
                    cx,
                )
            }))
        };

        workspace.toggle_modal(window, cx, |window, cx| {
            Self::new(store, selected_project, window, cx)
        });
    }

    fn new(
        store: Entity<TaskBoardStore>,
        mut selected_project: Option<BoardProjectId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let title_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Task title", window, cx);
            editor
        });
        let description_editor = cx.new(|cx| {
            let mut editor = Editor::auto_height(3, 12, window, cx);
            editor.set_placeholder_text("Description (optional)", window, cx);
            editor
        });

        let mut status = TaskStatus::Todo;
        let mut draft_restored = false;
        let draft: Option<NewTaskDraft> = KeyValueStore::global(cx)
            .read_kvp(NEW_TASK_DRAFT_KEY)
            .ok()
            .flatten()
            .and_then(|json| serde_json::from_str(&json).ok());
        if let Some(draft) = draft
            && (!draft.title.is_empty() || !draft.description.is_empty())
        {
            draft_restored = true;
            status = draft.status;
            title_editor.update(cx, |editor, cx| {
                editor.set_text(draft.title, window, cx);
            });
            description_editor.update(cx, |editor, cx| {
                editor.set_text(draft.description, window, cx);
            });
            if let Some(project_id) = draft.project_id
                && store.read(cx).project(project_id).is_some()
            {
                selected_project = Some(project_id);
            }
        }

        Self {
            store,
            title_editor,
            description_editor,
            selected_project,
            status,
            draft_restored,
            created: false,
        }
    }

    fn persist_draft(&self, cx: &mut Context<Self>) {
        let title = self.title_editor.read(cx).text(cx).trim().to_string();
        let description = self.description_editor.read(cx).text(cx).trim().to_string();
        let kvp = KeyValueStore::global(cx);
        if title.is_empty() && description.is_empty() {
            cx.background_spawn(async move {
                kvp.delete_kvp(NEW_TASK_DRAFT_KEY.to_string()).await
            })
            .detach_and_log_err(cx);
            return;
        }
        let draft = NewTaskDraft {
            title,
            description,
            status: self.status,
            project_id: self.selected_project,
        };
        if let Some(json) = serde_json::to_string(&draft).log_err() {
            cx.background_spawn(async move {
                kvp.write_kvp(NEW_TASK_DRAFT_KEY.to_string(), json).await
            })
            .detach_and_log_err(cx);
        }
    }

    fn discard_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.title_editor.update(cx, |editor, cx| {
            editor.set_text("", window, cx);
        });
        self.description_editor.update(cx, |editor, cx| {
            editor.set_text("", window, cx);
        });
        self.status = TaskStatus::Todo;
        self.draft_restored = false;
        let kvp = KeyValueStore::global(cx);
        cx.background_spawn(async move { kvp.delete_kvp(NEW_TASK_DRAFT_KEY.to_string()).await })
            .detach_and_log_err(cx);
        cx.notify();
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &menu::Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        self.create_task(cx);
    }

    fn create_task(&mut self, cx: &mut Context<Self>) {
        let title = self.title_editor.read(cx).text(cx);
        let title = title.trim();
        let Some(project_id) = self.selected_project else {
            return;
        };
        if title.is_empty() {
            return;
        }

        let description = self.description_editor.read(cx).text(cx);
        let description = (!description.trim().is_empty()).then(|| description.trim().to_string());

        self.store.update(cx, |store, cx| {
            store.create_task(project_id, title.to_string().into(), description, self.status, cx);
        });
        self.created = true;
        let kvp = KeyValueStore::global(cx);
        cx.background_spawn(async move { kvp.delete_kvp(NEW_TASK_DRAFT_KEY.to_string()).await })
            .detach_and_log_err(cx);
        cx.emit(DismissEvent);
    }

    fn render_project_dropdown(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let store = self.store.read(cx);
        let label = self
            .selected_project
            .and_then(|id| store.project(id))
            .map(|project| project.display_name.clone())
            .unwrap_or_else(|| "Select project".into());

        let projects: Vec<(BoardProjectId, SharedString)> = store
            .projects()
            .map(|project| (project.project_id, project.display_name.clone()))
            .collect();
        let this = cx.entity().downgrade();

        DropdownMenu::new(
            "new-task-project",
            label,
            ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
                for (project_id, name) in projects {
                    let this = this.clone();
                    menu = menu.entry(name, None, move |_window, cx| {
                        this.update(cx, |this, cx| {
                            this.selected_project = Some(project_id);
                            cx.notify();
                        })
                        .ok();
                    });
                }
                menu
            }),
        )
    }

    fn render_status_dropdown(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let this = cx.entity().downgrade();
        DropdownMenu::new(
            "new-task-status",
            self.status.label(),
            ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
                for status in TaskStatus::ALL {
                    let this = this.clone();
                    menu = menu.entry(status.label(), None, move |_window, cx| {
                        this.update(cx, |this, cx| {
                            this.status = status;
                            cx.notify();
                        })
                        .ok();
                    });
                }
                menu
            }),
        )
    }
}

impl Render for NewTaskModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let can_create = self.selected_project.is_some();

        v_flex()
            .key_context("NewTaskModal")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .elevation_3(cx)
            .w(rems(34.))
            .child(
                v_flex()
                    .p_3()
                    .gap_2()
                    .child(
                        h_flex()
                            .gap_2()
                            .justify_between()
                            .child(Label::new("New Task"))
                            .when(self.draft_restored, |this| {
                                this.child(
                                    h_flex()
                                        .gap_1()
                                        .child(
                                            Label::new("Draft restored")
                                                .size(LabelSize::XSmall)
                                                .color(Color::Muted),
                                        )
                                        .child(
                                            Button::new("discard-draft", "Discard")
                                                .label_size(LabelSize::XSmall)
                                                .on_click(cx.listener(
                                                    |this, _, window, cx| {
                                                        this.discard_draft(window, cx);
                                                    },
                                                )),
                                        ),
                                )
                            }),
                    )
                    .child(
                        div()
                            .p_2()
                            .rounded_md()
                            .bg(cx.theme().colors().editor_background)
                            .border_1()
                            .border_color(cx.theme().colors().border_variant)
                            .child(self.title_editor.clone()),
                    )
                    .child(
                        div()
                            .p_2()
                            .rounded_md()
                            .bg(cx.theme().colors().editor_background)
                            .border_1()
                            .border_color(cx.theme().colors().border_variant)
                            .child(self.description_editor.clone()),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(self.render_project_dropdown(window, cx))
                            .child(self.render_status_dropdown(window, cx)),
                    ),
            )
            .child(
                h_flex()
                    .p_2()
                    .gap_1()
                    .justify_end()
                    .border_t_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(Button::new("cancel", "Cancel").on_click(cx.listener(
                        |_, _, _window, cx| {
                            cx.emit(DismissEvent);
                        },
                    )))
                    .child(
                        Button::new("create", "Create Task")
                            .style(ButtonStyle::Filled)
                            .disabled(!can_create)
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.create_task(cx);
                            })),
                    ),
            )
    }
}

/// Register the current workspace's repository on the task board.
pub(crate) fn register_workspace_project(
    workspace: &mut Workspace,
    cx: &mut Context<Workspace>,
) -> Option<BoardProjectId> {
    let group_key = ProjectGroupKey::from_project(workspace.project().read(cx), cx);
    if group_key.path_list().is_empty() {
        return None;
    }
    let store = TaskBoardStore::global(cx);
    Some(store.update(cx, |store, cx| {
        store.register_project(
            group_key.path_list().clone(),
            group_key.host(),
            display_name_for_paths(group_key.path_list()),
            cx,
        )
    }))
}
