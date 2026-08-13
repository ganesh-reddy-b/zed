use db::kvp::KeyValueStore;
use editor::{Editor, EditorEvent};
use gpui::{
    Action as _, App, Context, Entity, EventEmitter, FocusHandle, Focusable, Render, ScrollHandle,
    SharedString, Subscription, Task, WeakEntity, Window,
};
use project::{ProjectGroupKey, TaskSourceKind};
use settings::Settings as _;
use task::{TaskContext, TaskTemplate};
use ui::{Chip, ContextMenu, DropdownMenu, Indicator, Tooltip, prelude::*, right_click_menu};
use workspace::{
    ItemId, Workspace, WorkspaceId, delete_unloaded_items,
    item::{Item, ItemEvent},
};

use crate::pr_sync::GhSetupStatus;

const GH_BANNER_DISMISSED_KEY: &str = "task_board_gh_setup_dismissed";

use crate::{
    BoardFilter, BoardProjectId, BoardTaskId, NewTask, TaskBoardSettings, TaskBoardStore,
    TaskStatus, task_store::TaskBoardStoreEvent,
};

/// Payload for a task card being dragged between or within columns. Also
/// serves as its own drag preview.
#[derive(Clone)]
struct DraggedTaskCard {
    task_id: BoardTaskId,
    title: SharedString,
    project_name: SharedString,
}

impl Render for DraggedTaskCard {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .p_2()
            .gap_0p5()
            .w(px(256.))
            .rounded_md()
            .bg(cx.theme().colors().elevated_surface_background)
            .border_1()
            .border_color(cx.theme().colors().border_focused)
            .shadow_md()
            .opacity(0.9)
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(cx.theme().colors().text_muted)
                    .truncate()
                    .child(self.project_name.clone()),
            )
            .child(
                Label::new(self.title.clone())
                    .size(LabelSize::Small)
                    .weight(gpui::FontWeight::MEDIUM)
                    .truncate(),
            )
    }
}

/// What the project dropdown is set to. `WorkspaceProjects` (the default)
/// shows tasks from every board project whose repository folders are part of
/// the current workspace.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum ProjectFilterChoice {
    #[default]
    WorkspaceProjects,
    AllProjects,
    Project(BoardProjectId),
}

pub struct TaskBoardView {
    focus_handle: FocusHandle,
    store: Entity<TaskBoardStore>,
    workspace: Option<WeakEntity<Workspace>>,
    columns: Vec<ColumnSnapshot>,
    project_choice: ProjectFilterChoice,
    filter: BoardFilter,
    filter_editor: Entity<Editor>,
    board_scroll_handle: ScrollHandle,
    gh_setup: Option<GhSetupStatus>,
    gh_banner_dismissed: bool,
    _subscriptions: Vec<Subscription>,
    /// Refreshes the workspace-projects filter when folders are added to or
    /// removed from the board's workspace; rebound on each workspace add.
    _project_subscription: Option<Subscription>,
    _pr_refresh_task: Task<()>,
}

struct ColumnSnapshot {
    status: TaskStatus,
    cards: Vec<CardSnapshot>,
}

struct CardSnapshot {
    task_id: BoardTaskId,
    title: SharedString,
    /// Primary project name, plus a "+N" suffix when the task spans more.
    project_name: SharedString,
    /// All project names (multi-project) or the worktree path, for hover.
    project_tooltip: Option<SharedString>,
    branch_name: Option<SharedString>,
    tags: Vec<SharedString>,
    prs: Vec<CardPr>,
    session_count: usize,
    running_sessions: usize,
    needs_attention: bool,
    archived: bool,
    has_worktree: bool,
    can_reopen: bool,
    /// The task's primary project has no git repository; the task runs in
    /// place, without a worktree or branch.
    non_git: bool,
}

#[derive(Clone)]
struct CardPr {
    url: SharedString,
    label: SharedString,
    tooltip: SharedString,
    color: Color,
}

/// A compact warning chip marking a project without a git repository.
pub(crate) fn non_git_badge(id: impl Into<gpui::ElementId>, cx: &App) -> impl IntoElement {
    h_flex()
        .id(id)
        .flex_none()
        .px_1()
        .rounded_sm()
        .border_1()
        .border_color(cx.theme().status().warning_border)
        .bg(cx.theme().status().warning_background)
        .tooltip(Tooltip::text(
            "This project is not a git repository. Its tasks run directly in the \
             project folder, without a worktree or branch.",
        ))
        .child(
            Label::new("no git")
                .size(LabelSize::XSmall)
                .color(Color::Warning),
        )
}

pub(crate) fn pr_state_color(state: crate::PrState) -> Color {
    match state {
        crate::PrState::Open => Color::Success,
        crate::PrState::Draft => Color::Muted,
        crate::PrState::Merged => Color::Accent,
        crate::PrState::Closed => Color::Error,
        crate::PrState::Unknown => Color::Muted,
    }
}

fn card_prs(store: &TaskBoardStore, task: &crate::BoardTask) -> Vec<CardPr> {
    let all_prs = store.prs_for_task(task.task_id);
    let mut prs: Vec<CardPr> = all_prs
        .iter()
        .filter(|pr| !pr.detached)
        .map(|pr| {
            let label: SharedString = match pr.number {
                Some(number) => format!("#{number}").into(),
                None => "PR".into(),
            };
            let tooltip: SharedString = match &pr.title {
                Some(title) => format!("{title} — {}", pr.state.label()).into(),
                None => pr.state.label().into(),
            };
            CardPr {
                url: pr.url.clone().into(),
                label,
                tooltip,
                color: pr_state_color(pr.state),
            }
        })
        .collect();
    // Tasks whose PR was recorded before status tracking (or created via the
    // compare page and never refreshed) still get a link chip — unless the
    // user detached everything.
    if all_prs.is_empty()
        && prs.is_empty()
        && let Some(pr_url) = &task.pr_url
    {
        prs.push(CardPr {
            url: pr_url.clone().into(),
            label: "PR".into(),
            tooltip: "Open pull request".into(),
            color: Color::Muted,
        });
    }
    prs
}

impl TaskBoardView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let store = TaskBoardStore::global(cx);
        let store_subscription =
            cx.subscribe(&store, |this: &mut Self, _, _: &TaskBoardStoreEvent, cx| {
                this.recompute(cx);
            });

        let filter_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Filter tasks…", window, cx);
            editor
        });
        let filter_subscription =
            cx.subscribe(&filter_editor, |this: &mut Self, editor, event, cx| {
                if let EditorEvent::BufferEdited = event {
                    this.filter.query = editor.read(cx).text(cx).into();
                    this.recompute(cx);
                }
            });
        // Column visibility comes from settings (hidden_statuses), so a
        // settings edit must recompute open boards.
        let settings_subscription =
            cx.observe_global::<settings::SettingsStore>(|this: &mut Self, cx| {
                this.recompute(cx);
            });

        // Keep PR statuses fresh while a board is visible; refreshes are
        // debounced in the store so multiple boards don't stack up.
        let _pr_refresh_task = cx.spawn(async move |this, cx| {
            loop {
                let refresh = cx.update(|cx| crate::pr_sync::refresh_all_prs(cx));
                refresh.await;
                cx.background_executor()
                    .timer(crate::pr_sync::REFRESH_INTERVAL)
                    .await;
                if this.upgrade().is_none() {
                    break;
                }
            }
        });

        // First-open setup check: is the GitHub CLI installed and signed in?
        let kvp = KeyValueStore::global(cx);
        cx.spawn(async move |this, cx| {
            let dismissed = kvp
                .read_kvp(GH_BANNER_DISMISSED_KEY)
                .ok()
                .flatten()
                .is_some();
            let status = crate::pr_sync::check_gh_setup().await;
            this.update(cx, |this, cx| {
                this.gh_banner_dismissed = dismissed;
                this.gh_setup = Some(status);
                cx.notify();
            })
            .ok();
        })
        .detach();

        let mut this = Self {
            focus_handle: cx.focus_handle(),
            store,
            workspace: None,
            columns: Vec::new(),
            project_choice: ProjectFilterChoice::default(),
            filter: BoardFilter::default(),
            filter_editor,
            board_scroll_handle: ScrollHandle::new(),
            gh_setup: None,
            gh_banner_dismissed: false,
            _subscriptions: vec![
                store_subscription,
                filter_subscription,
                settings_subscription,
            ],
            _project_subscription: None,
            _pr_refresh_task,
        };
        this.recompute(cx);
        this
    }

    fn recheck_gh_setup(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let status = crate::pr_sync::check_gh_setup().await;
            this.update(cx, |this, cx| {
                let became_ready =
                    this.gh_setup != Some(GhSetupStatus::Ready) && status == GhSetupStatus::Ready;
                this.gh_setup = Some(status);
                if became_ready {
                    this.store
                        .update(cx, |store, _| store.reset_pr_refresh_debounce());
                    crate::pr_sync::refresh_all_prs(cx).detach();
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn dismiss_gh_banner(&mut self, cx: &mut Context<Self>) {
        self.gh_banner_dismissed = true;
        let kvp = KeyValueStore::global(cx);
        cx.background_spawn(async move {
            kvp.write_kvp(GH_BANNER_DISMISSED_KEY.to_string(), "true".to_string())
                .await
        })
        .detach();
        cx.notify();
    }

    /// Run a setup command (install / sign in) in a visible task terminal,
    /// re-checking the setup status when it finishes.
    fn run_gh_setup_command(
        &mut self,
        label: &str,
        command: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.upgrade())
        else {
            return;
        };
        let template = TaskTemplate {
            label: label.to_string(),
            command,
            ..TaskTemplate::default()
        };
        let Some(resolved) = template.resolve_task(
            &TaskSourceKind::UserInput.to_id_base(),
            &TaskContext::default(),
        ) else {
            return;
        };

        let this = cx.entity().downgrade();
        workspace.update(cx, |workspace, cx| {
            workspace.schedule_resolved_task_with_completion(
                TaskSourceKind::UserInput,
                resolved,
                true,
                move |_result, cx| {
                    this.update(cx, |this, cx| this.recheck_gh_setup(cx)).ok();
                },
                window,
                cx,
            );
        });
    }

    fn render_gh_banner(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if self.gh_banner_dismissed {
            return None;
        }
        let status = self.gh_setup?;
        let (message, action_label, action_command): (&str, &str, String) = match status {
            GhSetupStatus::Ready => return None,
            GhSetupStatus::NotInstalled => (
                "Install the GitHub CLI (gh) to see pull request status on task cards.",
                "Install GitHub CLI",
                "brew install gh".to_string(),
            ),
            GhSetupStatus::NotAuthenticated => (
                "Sign in to GitHub to see pull request status on task cards.",
                "Sign In to GitHub",
                format!(
                    "{} auth login",
                    crate::pr_sync::gh_binary()
                        .map(|path| path.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "gh".to_string())
                ),
            ),
        };
        let action_label_owned = action_label.to_string();

        Some(
            h_flex()
                .p_2()
                .gap_2()
                .border_b_1()
                .border_color(cx.theme().status().warning_border)
                .bg(cx.theme().status().warning_background)
                .child(
                    Icon::new(IconName::Warning)
                        .size(IconSize::Small)
                        .color(Color::Warning),
                )
                .child(Label::new(message).size(LabelSize::Small))
                .child(div().flex_1())
                .child(
                    Button::new("gh-setup-action", action_label_owned.clone())
                        .style(ButtonStyle::Filled)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.run_gh_setup_command(
                                &action_label_owned,
                                action_command.clone(),
                                window,
                                cx,
                            );
                        })),
                )
                .child(
                    Button::new("gh-setup-recheck", "Check Again").on_click(cx.listener(
                        |this, _, _window, cx| {
                            this.recheck_gh_setup(cx);
                        },
                    )),
                )
                .child(
                    IconButton::new("gh-setup-dismiss", IconName::Close)
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text("Dismiss"))
                        .on_click(cx.listener(|this, _, _window, cx| {
                            this.dismiss_gh_banner(cx);
                        })),
                )
                .into_any_element(),
        )
    }

    fn recompute(&mut self, cx: &mut Context<Self>) {
        self.resolve_project_filter(cx);
        let hidden_statuses = &TaskBoardSettings::get_global(cx).hidden_statuses;
        let store = self.store.read(cx);

        self.columns = TaskStatus::ALL
            .into_iter()
            .filter(|status| !hidden_statuses.contains(status))
            .map(|status| ColumnSnapshot {
                status,
                cards: store
                    .tasks_for_column(status, &self.filter)
                    .into_iter()
                    .map(|task| {
                        let sessions = store.sessions_for_task(task.task_id);
                        let session_count = sessions
                            .iter()
                            .filter(|info| !info.session.archived)
                            .count();
                        let running_sessions = sessions
                            .iter()
                            .filter(|info| {
                                !info.session.archived
                                    && info.runtime != crate::SessionRuntimeState::Dormant
                            })
                            .count();

                        let primary_name = store
                            .project(task.project_id)
                            .map(|project| project.display_name.clone())
                            .unwrap_or_else(|| "Unknown project".into());
                        let extra_names: Vec<SharedString> = task
                            .extra_project_ids
                            .iter()
                            .filter_map(|project_id| store.project(*project_id))
                            .map(|project| project.display_name.clone())
                            .collect();
                        let project_name: SharedString = if extra_names.is_empty() {
                            primary_name.clone()
                        } else {
                            format!("{primary_name} +{}", extra_names.len()).into()
                        };
                        let project_tooltip: Option<SharedString> = if extra_names.is_empty() {
                            task.worktree_path().map(|path| {
                                SharedString::from(path.to_string_lossy().into_owned())
                            })
                        } else {
                            let mut names = vec![primary_name.to_string()];
                            names.extend(extra_names.iter().map(ToString::to_string));
                            Some(SharedString::from(names.join(", ")))
                        };

                        CardSnapshot {
                            task_id: task.task_id,
                            title: task.title.clone(),
                            project_name,
                            project_tooltip,
                            branch_name: task.branch_name.clone().map(SharedString::from),
                            tags: task.tags.clone(),
                            prs: card_prs(store, task),
                            session_count,
                            running_sessions,
                            needs_attention: store.task_needs_attention(task.task_id),
                            archived: task.archived,
                            has_worktree: task.has_worktree(),
                            can_reopen: !task.has_worktree()
                                && store
                                    .archived_worktrees_for_task(task.task_id)
                                    .next()
                                    .is_some(),
                            non_git: store.project_is_non_git(task.project_id),
                        }
                    })
                    .collect(),
            })
            .collect();
        cx.notify();
    }

    /// Move the dragged task into `status`, before the card `before` (end of
    /// the column when `None`). Anchoring on the target card rather than a
    /// visual index keeps drops correct while the board is filtered.
    fn move_dragged_task(
        &mut self,
        dragged: &DraggedTaskCard,
        status: TaskStatus,
        before: Option<BoardTaskId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let task_id = dragged.task_id;
        let needs_finish_flow = matches!(status, TaskStatus::Done | TaskStatus::Cancelled)
            && self
                .store
                .read(cx)
                .task(task_id)
                .is_some_and(|task| task.has_worktree() && !task.runs_in_place());

        if needs_finish_flow {
            run_workspace_task(&self.workspace, window, cx, |workspace, window, cx| {
                crate::task_lifecycle::finish_task(task_id, status, workspace, window, cx)
            });
        } else {
            self.store.update(cx, |store, cx| {
                store.move_task_before(task_id, status, before, cx);
            });
        }
    }

    /// Compact status-colored PR chips: up to three, then a "+N" chip that
    /// opens the task detail with the full list.
    fn render_pr_chips(&self, card: &CardSnapshot, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        const MAX_CHIPS: usize = 3;
        let task_id = card.task_id;
        let shown = if card.prs.len() > MAX_CHIPS {
            MAX_CHIPS - 1
        } else {
            card.prs.len()
        };
        // When a PR chip leads the footer row, pull its hover padding back so
        // the icon lines up with the card's left edge.
        let first_chip_is_flush = card.tags.is_empty() && card.session_count == 0;

        let mut chips = Vec::new();
        for (index, pr) in card.prs.iter().take(shown).enumerate() {
            let url = pr.url.clone();
            let tooltip = pr.tooltip.clone();
            chips.push(
                h_flex()
                    .id(SharedString::from(format!(
                        "task-pr-{}-{index}",
                        task_id.to_key_string()
                    )))
                    .gap_0p5()
                    .px_1()
                    .when(first_chip_is_flush && index == 0, |this| this.ml_neg_1())
                    .rounded_sm()
                    .hover(|style| style.bg(cx.theme().colors().element_hover))
                    .child(
                        Icon::new(IconName::PullRequest)
                            .size(IconSize::XSmall)
                            .color(pr.color),
                    )
                    .child(Label::new(pr.label.clone()).size(LabelSize::XSmall).color(pr.color))
                    .tooltip(Tooltip::text(tooltip))
                    .on_click(move |_, _window, cx| {
                        cx.stop_propagation();
                        cx.open_url(&url);
                    })
                    .into_any_element(),
            );
        }
        if card.prs.len() > shown {
            let hidden = card.prs.len() - shown;
            chips.push(
                h_flex()
                    .id(SharedString::from(format!(
                        "task-pr-more-{}",
                        task_id.to_key_string()
                    )))
                    .px_1()
                    .rounded_sm()
                    .hover(|style| style.bg(cx.theme().colors().element_hover))
                    .child(
                        Label::new(format!("+{hidden}"))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .tooltip(Tooltip::text("Show all pull requests"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.open_task_detail(task_id, window, cx);
                    }))
                    .into_any_element(),
            );
        }
        chips
    }

    /// Resolve the dropdown choice into a concrete project set for the
    /// store's filter. "Workspace projects" matches every board project with
    /// at least one repository folder in this board's workspace, so a
    /// workspace holding folders A, B, and C shows the tasks of all three.
    fn resolve_project_filter(&mut self, cx: &App) {
        self.filter.projects = match self.project_choice {
            ProjectFilterChoice::AllProjects => None,
            ProjectFilterChoice::Project(project_id) => Some(vec![project_id]),
            ProjectFilterChoice::WorkspaceProjects => {
                let workspace_paths: std::collections::HashSet<_> = self
                    .workspace
                    .as_ref()
                    .and_then(|workspace| workspace.upgrade())
                    .map(|workspace| {
                        ProjectGroupKey::from_project(workspace.read(cx).project().read(cx), cx)
                            .path_list()
                            .ordered_paths()
                            .cloned()
                            .collect()
                    })
                    .unwrap_or_default();
                Some(
                    self.store
                        .read(cx)
                        .projects()
                        .filter(|project| {
                            project
                                .main_worktree_paths
                                .ordered_paths()
                                .any(|path| workspace_paths.contains(path))
                        })
                        .map(|project| project.project_id)
                        .collect(),
                )
            }
        };
    }

    /// Persist a section's visibility to `task_board.hidden_statuses` in the
    /// user settings; the settings observer then refreshes every open board.
    fn set_status_hidden(&self, status: TaskStatus, hidden: bool, cx: &mut App) {
        let Some(workspace) = self
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.upgrade())
        else {
            return;
        };
        let fs = workspace.read(cx).project().read(cx).fs().clone();
        settings::update_settings_file(fs, cx, move |content, _| {
            let task_board = content.task_board.get_or_insert_default();
            let mut hidden_statuses = task_board.hidden_statuses.clone().unwrap_or_default();
            let key = status.as_str().to_string();
            if hidden {
                if !hidden_statuses.contains(&key) {
                    hidden_statuses.push(key);
                }
            } else {
                hidden_statuses.retain(|existing| existing != &key);
            }
            task_board.hidden_statuses = Some(hidden_statuses);
        });
    }

    /// A toolbar menu listing every section with a checkmark; unchecking
    /// hides it, and hidden sections can be re-enabled from here.
    fn render_sections_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let hidden_statuses = TaskBoardSettings::get_global(cx).hidden_statuses.clone();
        let this = cx.entity().downgrade();

        ui::PopoverMenu::new("board-sections")
            .trigger(
                IconButton::new("board-sections-button", IconName::ListCollapse)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Show or hide sections")),
            )
            .menu(move |window, cx| {
                let hidden_statuses = hidden_statuses.clone();
                let this = this.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
                    for status in TaskStatus::ALL {
                        let this = this.clone();
                        let currently_hidden = hidden_statuses.contains(&status);
                        menu = menu.toggleable_entry(
                            status.label(),
                            !currently_hidden,
                            IconPosition::Start,
                            None,
                            move |_window, cx| {
                                this.update(cx, |this, cx| {
                                    this.set_status_hidden(status, !currently_hidden, cx);
                                })
                                .ok();
                            },
                        );
                    }
                    menu
                }))
            })
    }

    fn open_task_detail(
        &mut self,
        task_id: BoardTaskId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.upgrade())
        else {
            return;
        };
        let weak_workspace = workspace.downgrade();
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                crate::CardDetailModal::new(weak_workspace, task_id, window, cx)
            });
        });
    }

    fn render_toolbar(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .p_2()
            .gap_2()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(Label::new("Task Board"))
            .children(self.workspace_non_git_badge(cx))
            .child(self.render_project_filter(window, cx))
            .child(self.render_sections_menu(cx))
            .child(
                IconButton::new("show-archived", IconName::Eye)
                    .icon_size(IconSize::Small)
                    .toggle_state(self.filter.show_archived)
                    .tooltip(Tooltip::text("Show archived tasks"))
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.filter.show_archived = !this.filter.show_archived;
                        this.recompute(cx);
                    })),
            )
            .child(div().flex_1())
            .child(
                div()
                    .w_64()
                    .px_2()
                    .py_0p5()
                    .rounded_md()
                    .bg(cx.theme().colors().editor_background)
                    .border_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(self.filter_editor.clone()),
            )
            .child(
                Button::new("new-task", "New Task")
                    .style(ButtonStyle::Filled)
                    .key_binding(ui::KeyBinding::for_action(&NewTask, cx))
                    .on_click(|_, window, cx| {
                        window.dispatch_action(NewTask.boxed_clone(), cx);
                    }),
            )
    }

    /// A persistent "no git" badge shown while the board's workspace is open
    /// on a project without a git repository.
    fn workspace_non_git_badge(&self, cx: &App) -> Option<gpui::AnyElement> {
        let workspace = self.workspace.as_ref()?.upgrade()?;
        let group_key = ProjectGroupKey::from_project(workspace.read(cx).project().read(cx), cx);
        if group_key.path_list().is_empty() {
            return None;
        }
        let store = self.store.read(cx);
        let project = store.project_for_group_key(&group_key)?;
        store
            .project_is_non_git(project.project_id)
            .then(|| non_git_badge("board-workspace-non-git", cx).into_any_element())
    }

    fn render_project_filter(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let store = self.store.read(cx);
        let label: SharedString = match self.project_choice {
            ProjectFilterChoice::WorkspaceProjects => "Workspace projects".into(),
            ProjectFilterChoice::AllProjects => "All projects".into(),
            ProjectFilterChoice::Project(project_id) => store
                .project(project_id)
                .map(|project| project.display_name.clone())
                .unwrap_or_else(|| "All projects".into()),
        };

        let projects: Vec<(BoardProjectId, SharedString)> = store
            .projects()
            .map(|project| (project.project_id, project.display_name.clone()))
            .collect();
        let this = cx.entity().downgrade();

        DropdownMenu::new(
            "board-project-filter",
            label,
            ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
                menu = menu.entry("Workspace projects", None, {
                    let this = this.clone();
                    move |_window, cx| {
                        set_project_filter(&this, ProjectFilterChoice::WorkspaceProjects, cx);
                    }
                });
                menu = menu.entry("All projects", None, {
                    let this = this.clone();
                    move |_window, cx| {
                        set_project_filter(&this, ProjectFilterChoice::AllProjects, cx);
                    }
                });
                menu = menu.separator();
                for (project_id, name) in projects {
                    let this = this.clone();
                    menu = menu.entry(name, None, move |_window, cx| {
                        set_project_filter(&this, ProjectFilterChoice::Project(project_id), cx);
                    });
                }
                menu
            }),
        )
    }

    fn render_column(
        &self,
        column_index: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let column = &self.columns[column_index];
        let status = column.status;
        let card_count = column.cards.len();

        v_flex()
            .id(SharedString::from(format!(
                "task-board-column-{}",
                status.as_str()
            )))
            .w(px(280.))
            .flex_none()
            .h_full()
            .rounded_md()
            .bg(cx.theme().colors().panel_background)
            .border_1()
            .border_color(cx.theme().colors().border)
            .child(
                h_flex()
                    .group(SharedString::from(format!(
                        "task-column-header-{}",
                        status.as_str()
                    )))
                    .p_2()
                    .gap_1()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(Label::new(status.label()).size(LabelSize::Small))
                    .child(
                        Label::new(card_count.to_string())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1())
                    .child(
                        IconButton::new(
                            SharedString::from(format!("hide-section-{}", status.as_str())),
                            IconName::EyeOff,
                        )
                        .icon_size(IconSize::XSmall)
                        .visible_on_hover(SharedString::from(format!(
                            "task-column-header-{}",
                            status.as_str()
                        )))
                        .tooltip(Tooltip::text("Hide this section"))
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            this.set_status_hidden(status, true, cx);
                        })),
                    ),
            )
            .child(
                v_flex()
                    .id(SharedString::from(format!(
                        "task-board-column-cards-{}",
                        status.as_str()
                    )))
                    .flex_1()
                    .p_2()
                    .gap_2()
                    .overflow_y_scroll()
                    .can_drop(|drag, _, _| drag.downcast_ref::<DraggedTaskCard>().is_some())
                    .drag_over::<DraggedTaskCard>(|style, _, _, cx| {
                        style.bg(cx.theme().colors().drop_target_background)
                    })
                    .on_drop(cx.listener(
                        move |this, dragged: &DraggedTaskCard, window, cx| {
                            // Dropping on the column body appends to the end.
                            this.move_dragged_task(dragged, status, None, window, cx);
                        },
                    ))
                    .when(card_count == 0, |this| {
                        this.child(
                            div().p_2().child(
                                Label::new("No tasks")
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                        )
                    })
                    .children((0..card_count).map(|card_index| {
                        self.render_card(column_index, card_index, cx)
                            .into_any_element()
                    })),
            )
    }

    fn render_card(
        &self,
        column_index: usize,
        card_index: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let column = &self.columns[column_index];
        let status = column.status;
        let card = &column.cards[card_index];
        let task_id = card.task_id;
        let key = task_id.to_key_string();
        let group_name = SharedString::from(format!("task-card-group-{key}"));
        let dragged_card = DraggedTaskCard {
            task_id,
            title: card.title.clone(),
            project_name: card.project_name.clone(),
        };

        let archived = card.archived;
        let has_worktree = card.has_worktree;
        let can_reopen = card.can_reopen;
        // Worktree tasks always carry a branch; a branchless task with paths
        // runs in place in a non-git project folder.
        let in_place = has_worktree && card.branch_name.is_none();

        let (primary_icon, primary_tooltip) = if has_worktree {
            (
                IconName::FolderOpen,
                if in_place { "Open Project" } else { "Open Worktree" },
            )
        } else if can_reopen {
            (IconName::HistoryRerun, "Reopen Task")
        } else {
            (IconName::PlayFilled, "Start Task")
        };

        let eyebrow = h_flex()
            .gap_1()
            .justify_between()
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1()
                    .child(
                        div()
                            .id(SharedString::from(format!("task-project-{key}")))
                            .min_w_0()
                            .text_size(px(10.))
                            .text_color(cx.theme().colors().text_muted)
                            .truncate()
                            .when_some(card.project_tooltip.clone(), |this, tooltip| {
                                this.tooltip(Tooltip::text(tooltip))
                            })
                            .child(card.project_name.clone()),
                    )
                    .when(card.non_git, |this| {
                        this.child(non_git_badge(
                            SharedString::from(format!("task-non-git-{key}")),
                            cx,
                        ))
                    }),
            )
            .child(
                h_flex()
                    .flex_none()
                    .gap_0p5()
                    .when(card.needs_attention, |this| {
                        this.child(
                            div()
                                .id(SharedString::from(format!("task-attention-{key}")))
                                .tooltip(Tooltip::text("A session is waiting for input"))
                                .child(Indicator::dot().color(Color::Warning)),
                        )
                    })
                    .child(
                        IconButton::new(
                            SharedString::from(format!("task-primary-{key}")),
                            primary_icon,
                        )
                        .icon_size(IconSize::XSmall)
                        .visible_on_hover(group_name.clone())
                        .tooltip(Tooltip::text(primary_tooltip))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            run_workspace_task(
                                &this.workspace,
                                window,
                                cx,
                                move |workspace, window, cx| {
                                    if has_worktree {
                                        crate::task_lifecycle::open_task_worktree(
                                            task_id, workspace, window, cx,
                                        )
                                    } else if can_reopen {
                                        crate::task_lifecycle::reopen_task(
                                            task_id, workspace, window, cx,
                                        )
                                    } else {
                                        crate::task_lifecycle::start_task(
                                            task_id, workspace, window, cx,
                                        )
                                    }
                                },
                            );
                        })),
                    )
                    .child(
                        ui::PopoverMenu::new(SharedString::from(format!("task-menu-{key}")))
                            .trigger(
                                IconButton::new(
                                    SharedString::from(format!("task-menu-button-{key}")),
                                    IconName::Ellipsis,
                                )
                                .icon_size(IconSize::XSmall)
                                .visible_on_hover(group_name.clone())
                                .tooltip(Tooltip::text("More actions")),
                            )
                            .menu({
                                let workspace = self.workspace.clone();
                                move |window, cx| {
                                    Some(build_card_menu(
                                        task_id,
                                        archived,
                                        has_worktree,
                                        in_place,
                                        can_reopen,
                                        workspace.clone(),
                                        window,
                                        cx,
                                    ))
                                }
                            }),
                    ),
            );

        let title = div().child(
            Label::new(card.title.clone())
                .size(LabelSize::Small)
                .weight(gpui::FontWeight::MEDIUM)
                .line_clamp(2),
        );

        let branch_row = card.branch_name.clone().map(|branch| {
            let copied_branch = branch.clone();
            let tooltip_text = format!("Click to copy: {branch}");
            h_flex().child(
                h_flex()
                    .id(SharedString::from(format!("task-branch-{key}")))
                    .min_w_0()
                    .gap_0p5()
                    // Horizontal padding gives the hover background breathing
                    // room; the negative margin keeps the icon flush with the
                    // card's left edge.
                    .px_0p5()
                    .ml_neg_0p5()
                    .rounded_sm()
                    .hover(|style| style.bg(cx.theme().colors().element_hover))
                    .tooltip(Tooltip::text(tooltip_text))
                    .child(
                        Icon::new(IconName::GitBranch)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(branch)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .truncate(),
                    )
                    .on_click(move |_, _window, cx| {
                        cx.stop_propagation();
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                            copied_branch.to_string(),
                        ));
                    }),
            )
        });

        let session_color = if card.running_sessions > 0 {
            Color::Success
        } else {
            Color::Muted
        };
        let sessions_word = if card.session_count == 1 {
            "session"
        } else {
            "sessions"
        };
        let session_tooltip = if card.running_sessions > 0 {
            format!(
                "{} {sessions_word} · {} running",
                card.session_count, card.running_sessions
            )
        } else {
            format!("{} {sessions_word}", card.session_count)
        };

        let card_element = v_flex()
            .id(SharedString::from(format!("task-card-{key}")))
            .group(group_name)
            .p_2()
            .gap_1()
            .rounded_md()
            .bg(cx.theme().colors().elevated_surface_background)
            .border_1()
            .border_color(cx.theme().colors().border_variant)
            .hover(|style| {
                style
                    .border_color(cx.theme().colors().border_focused)
                    .shadow_sm()
            })
            .cursor_pointer()
            .when(card.archived, |this| this.opacity(0.6))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_task_detail(task_id, window, cx);
            }))
            .on_drag(dragged_card, |dragged, _offset, _window, cx| {
                cx.new(|_| dragged.clone())
            })
            .can_drop(|drag, _, _| drag.downcast_ref::<DraggedTaskCard>().is_some())
            .drag_over::<DraggedTaskCard>(|style, _, _, cx| {
                style
                    .border_color(cx.theme().colors().drop_target_border)
                    .bg(cx.theme().colors().drop_target_background)
            })
            .on_drop(cx.listener(
                move |this, dragged: &DraggedTaskCard, window, cx| {
                    if dragged.task_id == task_id {
                        return;
                    }
                    this.move_dragged_task(dragged, status, Some(task_id), window, cx);
                },
            ))
            .child(eyebrow)
            .child(title)
            .children(branch_row)
            .when(
                !card.tags.is_empty() || card.session_count > 0 || !card.prs.is_empty(),
                |this| {
                    this.child(
                        h_flex()
                            .gap_1()
                            .flex_wrap()
                            .children(card.tags.iter().map(|tag| Chip::new(tag.clone())))
                            .when(card.session_count > 0, |this| {
                                this.child(
                                    h_flex()
                                        .id(SharedString::from(format!("task-sessions-{key}")))
                                        .gap_0p5()
                                        .tooltip(Tooltip::text(session_tooltip))
                                        .child(
                                            Icon::new(IconName::Terminal)
                                                .size(IconSize::XSmall)
                                                .color(session_color),
                                        )
                                        .child(
                                            Label::new(card.session_count.to_string())
                                                .size(LabelSize::XSmall)
                                                .color(session_color),
                                        ),
                                )
                            })
                            .children(self.render_pr_chips(card, cx)),
                    )
                },
            );

        let workspace = self.workspace.clone();
        right_click_menu(SharedString::from(format!("task-card-menu-{key}")))
            .trigger(move |_, _, _| card_element)
            .menu(move |window, cx| {
                build_card_menu(
                    task_id,
                    archived,
                    has_worktree,
                    in_place,
                    can_reopen,
                    workspace.clone(),
                    window,
                    cx,
                )
            })
    }
}

/// The card's action menu, shared between right-click and the hover `…`
/// button.
fn build_card_menu(
    task_id: BoardTaskId,
    archived: bool,
    has_worktree: bool,
    in_place: bool,
    can_reopen: bool,
    workspace: Option<WeakEntity<Workspace>>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ContextMenu> {
    ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
        if has_worktree {
            let workspace = workspace.clone();
            menu = menu.entry(
                if in_place { "Open Project" } else { "Open Worktree" },
                None,
                {
                    let workspace = workspace.clone();
                    move |window, cx| {
                        run_workspace_task(&workspace, window, cx, |workspace, window, cx| {
                            crate::task_lifecycle::open_task_worktree(
                                task_id, workspace, window, cx,
                            )
                        });
                    }
                },
            );
            // In-place tasks have no branch, so there is nothing to publish.
            if !in_place {
                menu = menu.entry("Create Pull Request", None, move |window, cx| {
                    run_workspace_task(&workspace, window, cx, |workspace, window, cx| {
                        crate::task_lifecycle::create_pull_request(task_id, workspace, window, cx)
                    });
                });
            }
        } else if can_reopen {
            let workspace = workspace.clone();
            menu = menu.entry("Reopen Task", None, move |window, cx| {
                run_workspace_task(&workspace, window, cx, |workspace, window, cx| {
                    crate::task_lifecycle::reopen_task(task_id, workspace, window, cx)
                });
            });
        } else {
            let workspace = workspace.clone();
            menu = menu.entry("Start Task", None, move |window, cx| {
                run_workspace_task(&workspace, window, cx, |workspace, window, cx| {
                    crate::task_lifecycle::start_task(task_id, workspace, window, cx)
                });
            });
        }
        menu = menu.separator();
        for status in TaskStatus::ALL {
            let workspace = workspace.clone();
            menu = menu.entry(
                format!("Move to {}", status.label()),
                None,
                move |window, cx| {
                    run_workspace_task(&workspace, window, cx, |workspace, window, cx| {
                        crate::task_lifecycle::request_status_change(
                            task_id, status, workspace, window, cx,
                        )
                    });
                },
            );
        }
        menu = menu.separator();
        menu = menu.entry(
            if archived { "Unarchive" } else { "Archive" },
            None,
            move |_window, cx| {
                TaskBoardStore::global(cx).update(cx, |store, cx| {
                    store.set_task_archived(task_id, !archived, cx);
                });
            },
        );
        menu = menu.entry("Delete", None, move |window, cx| {
            run_workspace_task(&workspace, window, cx, |workspace, window, cx| {
                crate::task_lifecycle::delete_task(task_id, workspace, window, cx)
            });
        });
        menu
    })
}

fn set_project_filter(
    this: &WeakEntity<TaskBoardView>,
    choice: ProjectFilterChoice,
    cx: &mut App,
) {
    this.update(cx, |this, cx| {
        this.project_choice = choice;
        this.recompute(cx);
    })
    .ok();
}

/// Run a workspace-scoped lifecycle task from a card menu handler, surfacing
/// failures as workspace error notifications.
fn run_workspace_task(
    workspace: &Option<WeakEntity<Workspace>>,
    window: &mut Window,
    cx: &mut App,
    f: impl FnOnce(
        &mut Workspace,
        &mut Window,
        &mut Context<Workspace>,
    ) -> Task<anyhow::Result<()>>,
) {
    let Some(workspace) = workspace.as_ref().and_then(|workspace| workspace.upgrade()) else {
        return;
    };
    workspace.update(cx, |workspace, cx| {
        let task = f(workspace, window, cx);
        cx.spawn(async move |workspace, cx| {
            if let Err(error) = task.await {
                workspace
                    .update(cx, |workspace, cx| {
                        workspace.show_error(error, cx);
                    })
                    .ok();
            }
        })
        .detach();
    });
}

impl Render for TaskBoardView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("TaskBoard")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(self.render_toolbar(window, cx))
            .children(self.render_gh_banner(cx))
            .child(
                div()
                    .id("task-board-columns")
                    .flex_1()
                    .p_2()
                    .overflow_x_scroll()
                    .track_scroll(&self.board_scroll_handle)
                    .child(h_flex().h_full().gap_2().items_start().children(
                        (0..self.columns.len()).map(|column_index| {
                            self.render_column(column_index, cx).into_any_element()
                        }),
                    )),
            )
    }
}

impl Focusable for TaskBoardView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<ItemEvent> for TaskBoardView {}

impl Item for TaskBoardView {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Task Board".into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::ListTodo))
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn can_split(&self) -> bool {
        true
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.workspace = Some(workspace.weak_handle());
        // Register the workspace's project so the board can always tell (and
        // show) whether the project being viewed is a git repository.
        let group_key = ProjectGroupKey::from_project(workspace.project().read(cx), cx);
        if !group_key.path_list().is_empty() {
            self.store.update(cx, |store, cx| {
                store.register_project(
                    group_key.path_list().clone(),
                    group_key.host(),
                    crate::new_task_modal::display_name_for_paths(group_key.path_list()),
                    cx,
                );
            });
        }
        // Folders added to or removed from the workspace change what the
        // workspace-projects filter matches.
        self._project_subscription = Some(cx.subscribe(
            workspace.project(),
            |this: &mut Self, _, event, cx| match event {
                project::Event::WorktreeAdded(_)
                | project::Event::WorktreeRemoved(_)
                | project::Event::WorktreeOrderChanged => {
                    this.recompute(cx);
                }
                _ => {}
            },
        ));
        // The workspace-projects filter can only resolve once the workspace
        // handle exists — but this hook runs inside the workspace's own
        // update, so reading it back through the handle must wait until
        // that update finishes.
        let this = cx.entity().downgrade();
        cx.defer(move |cx| {
            this.update(cx, |this, cx| this.recompute(cx)).ok();
        });
    }

    fn clone_on_split(
        &self,
        _workspace_id: Option<WorkspaceId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Self>>> {
        Task::ready(Some(cx.new(|cx| TaskBoardView::new(window, cx))))
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

impl workspace::SerializableItem for TaskBoardView {
    fn serialized_item_kind() -> &'static str {
        "TaskBoard"
    }

    fn cleanup(
        workspace_id: WorkspaceId,
        alive_items: Vec<ItemId>,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<()>> {
        delete_unloaded_items(
            alive_items,
            workspace_id,
            "task_board_views",
            &persistence::TaskBoardViewsDb::global(cx),
            cx,
        )
    }

    fn deserialize(
        _project: Entity<project::Project>,
        _workspace: WeakEntity<Workspace>,
        workspace_id: WorkspaceId,
        item_id: ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<Entity<Self>>> {
        let db = persistence::TaskBoardViewsDb::global(cx);
        window.spawn(cx, async move |cx| {
            if db.get_task_board_view(item_id, workspace_id)?.is_some() {
                cx.update(|window, cx| cx.new(|cx| TaskBoardView::new(window, cx)))
            } else {
                Err(anyhow::anyhow!("no task board view to deserialize"))
            }
        })
    }

    fn serialize(
        &mut self,
        workspace: &mut Workspace,
        item_id: ItemId,
        _closing: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Task<anyhow::Result<()>>> {
        let workspace_id = workspace.database_id()?;
        let db = persistence::TaskBoardViewsDb::global(cx);
        Some(cx.background_spawn(async move {
            db.save_task_board_view(item_id, workspace_id).await
        }))
    }

    fn should_serialize(&self, event: &Self::Event) -> bool {
        event == &ItemEvent::UpdateTab
    }
}

mod persistence {
    use db::{
        query,
        sqlez::{domain::Domain, thread_safe_connection::ThreadSafeConnection},
        sqlez_macros::sql,
    };
    use workspace::WorkspaceDb;

    pub struct TaskBoardViewsDb(ThreadSafeConnection);

    impl Domain for TaskBoardViewsDb {
        const NAME: &str = stringify!(TaskBoardViewsDb);

        const MIGRATIONS: &[&str] = &[sql!(
            CREATE TABLE task_board_views (
                workspace_id INTEGER,
                item_id INTEGER UNIQUE,

                PRIMARY KEY(workspace_id, item_id),
                FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
                ON DELETE CASCADE
            ) STRICT;
        )];
    }

    db::static_connection!(TaskBoardViewsDb, [WorkspaceDb]);

    impl TaskBoardViewsDb {
        query! {
            pub async fn save_task_board_view(
                item_id: workspace::ItemId,
                workspace_id: workspace::WorkspaceId
            ) -> Result<()> {
                INSERT OR REPLACE INTO task_board_views(item_id, workspace_id)
                VALUES (?, ?)
            }
        }

        query! {
            pub fn get_task_board_view(
                item_id: workspace::ItemId,
                workspace_id: workspace::WorkspaceId
            ) -> Result<Option<workspace::ItemId>> {
                SELECT item_id
                FROM task_board_views
                WHERE item_id = ? AND workspace_id = ?
            }
        }
    }
}
