use editor::Editor;
use gpui::{
    App, AppContext as _, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render,
    SharedString, Subscription, TaskExt as _, WeakEntity, Window,
};
use settings::Settings as _;
use ui::{ContextMenu, Divider, DropdownMenu, Indicator, Tooltip, prelude::*};
use workspace::{ModalView, Workspace};

use crate::{
    BoardTaskId, SessionRuntimeState, TaskBoardSettings, TaskBoardStore, TaskSessionInfo,
    TaskStatus, session_manager, task_store::TaskBoardStoreEvent,
};

pub struct CardDetailModal {
    store: Entity<TaskBoardStore>,
    workspace: WeakEntity<Workspace>,
    task_id: BoardTaskId,
    title_editor: Entity<Editor>,
    description_editor: Entity<Editor>,
    tags_editor: Entity<Editor>,
    pr_refresh_error: Option<SharedString>,
    _store_subscription: Subscription,
}

impl EventEmitter<DismissEvent> for CardDetailModal {}
impl ModalView for CardDetailModal {}

impl Focusable for CardDetailModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.description_editor.focus_handle(cx)
    }
}

impl CardDetailModal {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        task_id: BoardTaskId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let store = TaskBoardStore::global(cx);
        let task = store.read(cx).task(task_id).cloned();

        let title_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Task title", window, cx);
            if let Some(task) = task.as_ref() {
                editor.set_text(task.title.to_string(), window, cx);
            }
            editor
        });
        let description_editor = cx.new(|cx| {
            let mut editor = Editor::auto_height(3, 12, window, cx);
            editor.set_placeholder_text("Description", window, cx);
            if let Some(description) = task.as_ref().and_then(|task| task.description.clone()) {
                editor.set_text(description, window, cx);
            }
            editor
        });
        let tags_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Tags (comma separated)", window, cx);
            if let Some(task) = task.as_ref() {
                let tags = task
                    .tags
                    .iter()
                    .map(SharedString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                editor.set_text(tags, window, cx);
            }
            editor
        });

        let _store_subscription =
            cx.subscribe(&store, |_, _, _: &TaskBoardStoreEvent, cx| cx.notify());

        // Opening a task's details is a natural moment to check its PRs.
        crate::pr_sync::refresh_task_prs(task_id, cx).detach_and_log_err(cx);

        Self {
            store,
            workspace,
            task_id,
            title_editor,
            description_editor,
            tags_editor,
            pr_refresh_error: None,
            _store_subscription,
        }
    }

    fn refresh_prs(&mut self, cx: &mut Context<Self>) {
        self.pr_refresh_error = None;
        let refresh = crate::pr_sync::refresh_task_prs(self.task_id, cx);
        cx.spawn(async move |this, cx| {
            let result = refresh.await;
            this.update(cx, |this, cx| {
                if let Err(error) = result {
                    this.pr_refresh_error = Some(format!("{error:#}").into());
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn render_pr_row(
        &self,
        pr: &crate::TaskPullRequest,
        index: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let url = SharedString::from(pr.url.clone());
        let color = crate::board_view::pr_state_color(pr.state);
        let title: SharedString = match (&pr.number, &pr.title) {
            (Some(number), Some(title)) => format!("#{number} {title}").into(),
            (Some(number), None) => format!("#{number}").into(),
            (None, Some(title)) => title.clone().into(),
            (None, None) => pr.url.clone().into(),
        };

        let task_id = self.task_id;
        let detached = pr.detached;
        let detach_url = pr.url.clone();

        h_flex()
            .gap_2()
            .p_1()
            .rounded_md()
            .hover(|style| style.bg(cx.theme().colors().element_hover))
            .when(detached, |this| this.opacity(0.5))
            .child(
                Icon::new(IconName::PullRequest)
                    .size(IconSize::Small)
                    .color(color),
            )
            .child(Label::new(title).size(LabelSize::Small))
            .child(
                Label::new(if detached { "detached" } else { pr.state.label() })
                    .size(LabelSize::XSmall)
                    .color(if detached { Color::Muted } else { color }),
            )
            .child(div().flex_1())
            .child(
                IconButton::new(
                    SharedString::from(format!("open-pr-{index}")),
                    IconName::ArrowUpRight,
                )
                .icon_size(IconSize::Small)
                .tooltip(Tooltip::text("Open in browser"))
                .on_click(move |_, _window, cx| {
                    cx.open_url(&url);
                }),
            )
            .child(
                IconButton::new(
                    SharedString::from(format!("detach-pr-{index}")),
                    if detached { IconName::Plus } else { IconName::Close },
                )
                .icon_size(IconSize::Small)
                .tooltip(Tooltip::text(if detached {
                    "Re-attach this pull request to the task"
                } else {
                    "Detach from this task (hides it from the card; survives refreshes)"
                }))
                .on_click(cx.listener(move |this, _, _window, cx| {
                    let detach_url = detach_url.clone();
                    this.store.update(cx, |store, cx| {
                        store.set_pr_detached(task_id, &detach_url, !detached, cx);
                    });
                })),
            )
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &menu::Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        self.save(cx);
        cx.emit(DismissEvent);
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let title = self.title_editor.read(cx).text(cx).trim().to_string();
        let description = self.description_editor.read(cx).text(cx);
        let description = (!description.trim().is_empty()).then(|| description.trim().to_string());
        let tags: Vec<SharedString> = self
            .tags_editor
            .read(cx)
            .text(cx)
            .split(',')
            .map(str::trim)
            .filter(|tag| !tag.is_empty())
            .map(|tag| SharedString::from(tag.to_string()))
            .collect();

        let task_id = self.task_id;
        self.store.update(cx, |store, cx| {
            store.update_task(
                task_id,
                |task| {
                    // An emptied title field means "keep the old title" so a
                    // stray select-all + delete can't wipe the task's name.
                    if !title.is_empty() {
                        task.title = title.into();
                    }
                    task.description = description;
                    task.tags = tags;
                },
                cx,
            );
        });
    }

    fn run_in_workspace(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
        f: impl FnOnce(
            &mut Workspace,
            &mut Window,
            &mut Context<Workspace>,
        ) -> gpui::Task<anyhow::Result<()>>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            let task = f(workspace, window, cx);
            cx.spawn(async move |workspace, cx| {
                if let Err(error) = task.await {
                    workspace
                        .update(cx, |workspace, cx| workspace.show_error(error, cx))
                        .ok();
                }
            })
            .detach();
        });
        cx.emit(DismissEvent);
    }

    fn render_status_dropdown(
        &self,
        status: TaskStatus,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let task_id = self.task_id;
        let this = cx.entity().downgrade();
        DropdownMenu::new(
            "card-detail-status",
            status.label(),
            ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
                for status in TaskStatus::ALL {
                    let this = this.clone();
                    menu = menu.entry(status.label(), None, move |window, cx| {
                        this.update(cx, |this, cx| {
                            this.run_in_workspace(window, cx, move |workspace, window, cx| {
                                crate::task_lifecycle::request_status_change(
                                    task_id, status, workspace, window, cx,
                                )
                            });
                        })
                        .ok();
                    });
                }
                menu
            }),
        )
    }

    fn render_new_session_dropdown(
        &self,
        has_worktree: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut agents: Vec<String> = TaskBoardSettings::get_global(cx)
            .agents
            .keys()
            .cloned()
            .collect();
        agents.sort();
        let task_id = self.task_id;
        let this = cx.entity().downgrade();

        ui::PopoverMenu::new("card-detail-new-session")
            .trigger(
                IconButton::new("new-session-button", IconName::Plus)
                    .icon_size(IconSize::Small)
                    .disabled(!has_worktree)
                    .tooltip(Tooltip::text(if has_worktree {
                        "New agent session in the task's worktree"
                    } else {
                        "Start the task first to create its worktree"
                    })),
            )
            .menu(move |window, cx| {
                let agents = agents.clone();
                let this = this.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
                    for agent in agents {
                        let this = this.clone();
                        menu = menu.entry(agent.clone(), None, move |window, cx| {
                            let agent = agent.clone();
                            this.update(cx, |this, cx| {
                                this.run_in_workspace(window, cx, move |workspace, window, cx| {
                                    session_manager::spawn_session(
                                        task_id, agent, workspace, window, cx,
                                    )
                                });
                            })
                            .ok();
                        });
                    }
                    menu
                }))
            })
    }

    /// A `+` menu of registered projects not yet on the task.
    fn render_add_project_dropdown(
        &self,
        task: &crate::BoardTask,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let attached: std::collections::HashSet<crate::BoardProjectId> =
            task.all_project_ids().collect();
        let available: Vec<(crate::BoardProjectId, SharedString)> = self
            .store
            .read(cx)
            .projects()
            .filter(|project| !attached.contains(&project.project_id))
            .map(|project| (project.project_id, project.display_name.clone()))
            .collect();
        let no_candidates = available.is_empty();
        let task_id = self.task_id;
        let this = cx.entity().downgrade();

        ui::PopoverMenu::new("card-detail-add-project")
            .trigger(
                IconButton::new("add-project-button", IconName::Plus)
                    .icon_size(IconSize::Small)
                    .disabled(no_candidates)
                    .tooltip(Tooltip::text(if no_candidates {
                        "Every registered project is already on this task"
                    } else {
                        "Add a project to this task"
                    })),
            )
            .menu(move |window, cx| {
                let available = available.clone();
                let this = this.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
                    for (project_id, name) in available {
                        let this = this.clone();
                        menu = menu.entry(name, None, move |_window, cx| {
                            this.update(cx, |this, cx| {
                                this.store.update(cx, |store, cx| {
                                    store.update_task(
                                        task_id,
                                        |task| task.extra_project_ids.push(project_id),
                                        cx,
                                    );
                                });
                            })
                            .ok();
                        });
                    }
                    menu
                }))
            })
    }

    fn render_project_row(
        &self,
        project_id: crate::BoardProjectId,
        is_primary: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let name = self
            .store
            .read(cx)
            .project(project_id)
            .map(|project| project.display_name.clone())
            .unwrap_or_else(|| "Unknown project".into());
        let task_id = self.task_id;

        h_flex()
            .gap_2()
            .p_1()
            .rounded_md()
            .hover(|style| style.bg(cx.theme().colors().element_hover))
            .child(
                Icon::new(IconName::Folder)
                    .size(IconSize::Small)
                    .color(Color::Muted),
            )
            .child(Label::new(name).size(LabelSize::Small))
            .when(is_primary, |this| {
                this.child(
                    Label::new("primary")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
            })
            .child(div().flex_1())
            .when(!is_primary, |this| {
                this.child(
                    IconButton::new(
                        SharedString::from(format!(
                            "remove-project-{}",
                            project_id.to_key_string()
                        )),
                        IconName::Close,
                    )
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Remove this project from the task"))
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        this.store.update(cx, |store, cx| {
                            store.update_task(
                                task_id,
                                |task| {
                                    task.extra_project_ids
                                        .retain(|extra| *extra != project_id);
                                },
                                cx,
                            );
                        });
                    })),
                )
            })
    }

    fn render_session_row(
        &self,
        info: &TaskSessionInfo,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let session_id = info.session.session_id;
        let (indicator_color, state_label) = match info.runtime {
            SessionRuntimeState::Running => (Color::Success, "running"),
            SessionRuntimeState::NeedsAttention => (Color::Warning, "needs attention"),
            SessionRuntimeState::Dormant => (Color::Muted, if info.session.archived {
                "archived"
            } else {
                "not running"
            }),
        };
        let label: SharedString = match &info.session.label {
            Some(label) => format!("{} — {}", info.session.agent, label).into(),
            None => info.session.agent.clone().into(),
        };
        let archived = info.session.archived;

        h_flex()
            .gap_2()
            .p_1()
            .rounded_md()
            .hover(|style| style.bg(cx.theme().colors().element_hover))
            .child(Indicator::dot().color(indicator_color))
            .child(
                Icon::new(IconName::Terminal)
                    .size(IconSize::Small)
                    .color(Color::Muted),
            )
            .child(Label::new(label).size(LabelSize::Small))
            .child(
                Label::new(state_label)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .child(div().flex_1())
            .child(
                IconButton::new(
                    SharedString::from(format!("open-session-{}", session_id.to_key_string())),
                    IconName::ArrowUpRight,
                )
                .icon_size(IconSize::Small)
                .tooltip(Tooltip::text(if archived {
                    "Restore session"
                } else {
                    "Open session"
                }))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.run_in_workspace(window, cx, move |workspace, window, cx| {
                        session_manager::open_session(session_id, workspace, window, cx)
                    });
                })),
            )
            .when(!archived, |this| {
                this.child(
                    IconButton::new(
                        SharedString::from(format!(
                            "archive-session-{}",
                            session_id.to_key_string()
                        )),
                        IconName::Archive,
                    )
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Archive session"))
                    .on_click(cx.listener(move |_, _, window, cx| {
                        session_manager::archive_session(session_id, window, cx);
                        cx.notify();
                    })),
                )
            })
    }
}

fn section_label(text: &'static str) -> Label {
    Label::new(text).size(LabelSize::XSmall).color(Color::Muted)
}

impl Render for CardDetailModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(task) = self.store.read(cx).task(self.task_id).cloned() else {
            cx.emit(DismissEvent);
            return v_flex().into_any_element();
        };
        let sessions = self.store.read(cx).sessions_for_task(self.task_id);
        let prs: Vec<crate::TaskPullRequest> =
            self.store.read(cx).prs_for_task(self.task_id).to_vec();
        let project_name = self
            .store
            .read(cx)
            .project(task.project_id)
            .map(|project| project.display_name.clone())
            .unwrap_or_else(|| "Unknown project".into());
        let task_id = self.task_id;

        let has_worktree = task.has_worktree();
        let can_reopen = !has_worktree
            && self
                .store
                .read(cx)
                .archived_worktrees_for_task(task_id)
                .next()
                .is_some();
        let (primary_icon, primary_label) = if has_worktree {
            (IconName::FolderOpen, "Open Worktree")
        } else if can_reopen {
            (IconName::HistoryRerun, "Reopen Task")
        } else {
            (IconName::PlayFilled, "Start Task")
        };
        let worktree_path = (!task.worktree_paths.is_empty()).then(|| {
            SharedString::from(
                task.worktree_paths
                    .ordered_paths()
                    .map(|path| path.to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join(", "),
            )
        });
        let created = format!("Created {}", task.created_at.format("%b %-d, %Y"));

        let header = v_flex()
            .p_3()
            .gap_1p5()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                h_flex()
                    .gap_2()
                    .child(div().flex_1().min_w_0().child(self.title_editor.clone()))
                    .child(self.render_status_dropdown(task.status, window, cx)),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Label::new(project_name)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .when_some(task.branch_name.clone(), |this, branch| {
                        let copied_branch = branch.clone();
                        this.child(
                            h_flex()
                                .id("card-detail-branch")
                                .min_w_0()
                                .gap_0p5()
                                .px_0p5()
                                .rounded_sm()
                                .hover(|style| style.bg(cx.theme().colors().element_hover))
                                .tooltip(Tooltip::text(format!("Click to copy: {branch}")))
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
                                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                        copied_branch.clone(),
                                    ));
                                }),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        Label::new(created)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            );

        let details_section = v_flex()
            .p_3()
            .gap_1()
            .child(section_label("Description"))
            .child(
                div()
                    .p_2()
                    .rounded_md()
                    .bg(cx.theme().colors().editor_background)
                    .border_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(self.description_editor.clone()),
            )
            .child(div().h_1())
            .child(section_label("Tags"))
            .child(
                div()
                    .p_2()
                    .rounded_md()
                    .bg(cx.theme().colors().editor_background)
                    .border_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(self.tags_editor.clone()),
            );

        let projects_section = v_flex()
            .p_3()
            .gap_1()
            .child(
                h_flex()
                    .gap_1()
                    .justify_between()
                    .child(section_label("Projects"))
                    .child(self.render_add_project_dropdown(&task, cx)),
            )
            .child(self.render_project_row(task.project_id, true, cx))
            .children(
                task.extra_project_ids
                    .iter()
                    .map(|project_id| {
                        self.render_project_row(*project_id, false, cx)
                            .into_any_element()
                    })
                    .collect::<Vec<_>>(),
            )
            .when(has_worktree && !task.extra_project_ids.is_empty(), |this| {
                this.child(
                    Label::new(
                        "Worktrees are created when the task starts; projects added \
                         later get theirs the next time the task is started.",
                    )
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
                )
            });

        let pr_section = v_flex()
            .p_3()
            .gap_1()
            .child(
                h_flex()
                    .gap_1()
                    .justify_between()
                    .child(section_label("Pull Requests"))
                    .child(
                        h_flex()
                            .gap_1()
                            .when(task.branch_name.is_some(), |this| {
                                this.child(
                                    IconButton::new("create-pr", IconName::Plus)
                                        .icon_size(IconSize::Small)
                                        .tooltip(Tooltip::text(
                                            "Create pull request: push the branch \
                                             and open your git host's PR page",
                                        ))
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.run_in_workspace(
                                                window,
                                                cx,
                                                move |workspace, window, cx| {
                                                    crate::task_lifecycle::create_pull_request(
                                                        task_id, workspace, window, cx,
                                                    )
                                                },
                                            );
                                        })),
                                )
                            })
                            .child(
                                IconButton::new("refresh-prs", IconName::RotateCcw)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("Refresh pull request status"))
                                    .on_click(cx.listener(|this, _, _window, cx| {
                                        this.refresh_prs(cx);
                                    })),
                            ),
                    ),
            )
            .when_some(self.pr_refresh_error.clone(), |this, error| {
                this.child(
                    Label::new(error)
                        .size(LabelSize::XSmall)
                        .color(Color::Error),
                )
            })
            .when(prs.is_empty(), |this| {
                this.child(
                    Label::new("No pull requests for this task's branch yet")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .children(
                prs.iter()
                    .enumerate()
                    .map(|(index, pr)| self.render_pr_row(pr, index, cx).into_any_element()),
            );

        let sessions_section = v_flex()
            .p_3()
            .gap_1()
            .child(
                h_flex()
                    .gap_1()
                    .justify_between()
                    .child(section_label("Sessions"))
                    .child(self.render_new_session_dropdown(has_worktree, window, cx)),
            )
            .when(sessions.is_empty(), |this| {
                this.child(
                    Label::new("No sessions yet")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .children(
                sessions
                    .iter()
                    .map(|info| self.render_session_row(info, cx).into_any_element()),
            );

        v_flex()
            .key_context("CardDetailModal")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .elevation_3(cx)
            .w(rems(40.))
            .max_h(vh(0.85, window))
            .child(header)
            .child(
                v_flex()
                    .id("card-detail-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(details_section)
                    .child(Divider::horizontal())
                    .child(projects_section)
                    .child(Divider::horizontal())
                    .child(pr_section)
                    .child(Divider::horizontal())
                    .child(sessions_section),
            )
            .child(
                h_flex()
                    .p_2()
                    .gap_1()
                    .border_t_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(
                        Button::new("detail-primary-action", primary_label)
                            .start_icon(Icon::new(primary_icon).size(IconSize::Small))
                            .when_some(worktree_path, |this, path| {
                                this.tooltip(Tooltip::text(path))
                            })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.run_in_workspace(window, cx, move |workspace, window, cx| {
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
                                });
                            })),
                    )
                    .child(div().flex_1())
                    .child(Button::new("close", "Cancel").on_click(cx.listener(
                        |_, _, _window, cx| {
                            cx.emit(DismissEvent);
                        },
                    )))
                    .child(
                        Button::new("save", "Save")
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.save(cx);
                                cx.emit(DismissEvent);
                            })),
                    ),
            )
            .into_any_element()
    }
}
