//! Projects sidebar: the projects list, the global Sessions list, and the
//! add-project palette (⌘K-style filtered folder browser).
//!
//! A project is a local folder and its sessions. Child module of `shell` so it
//! renders straight off `Shell`'s private state.

use super::*;
use crate::motion::TAB_SLIDE;
use crate::pickers::{breadcrumbs, browser_rows, parent_path};
use crate::terminal::panel::{drop_index, reorder_tabs, slide_offset};
use comet_proto::{ChatIndicator, Device, FolderListing, Project};
use gpui::FocusHandle;

/// Project-row slot height for drag drop-index math: py(6)×2 + 17px line ≈ 29,
/// plus the 2px column gap.
const PROJECT_ROW_SLOT: f32 = 31.0;

/// Drag-reorder state for the projects list; `epoch` keys the 150ms slide
/// animation restarts (the session-tab idiom, vertical).
pub(super) struct ProjectDragState {
    from: usize,
    over: usize,
    epoch: usize,
    prev_over: usize,
}

/// The dragged-row payload (gpui drag-and-drop).
struct ProjectDragPayload {
    from: usize,
    name: SharedString,
}

/// The floating row rendered at the cursor while dragging.
struct ProjectGhost {
    name: SharedString,
}

impl Render for ProjectGhost {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        div()
            .w(px(200.0))
            .h(px(29.0))
            .px(px(Theme::SPACE_SM))
            .flex()
            .items_center()
            .gap(px(Theme::SPACE_SM))
            .rounded(px(8.0))
            .bg(theme.surface_raised)
            .border_1()
            .border_color(theme.border_strong)
            .text_size(px(13.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(theme.text)
            .opacity(0.85)
            .child(
                icon(icons::FOLDER)
                    .size(px(16.0))
                    .flex_none()
                    .text_color(theme.text_muted),
            )
            .child(div().truncate().child(self.name.clone()))
    }
}

/// The add-project palette (a command-K surface, summoned by ⌘K): search bar,
/// local folder browser, and kbd-hint footer.
pub(super) struct CreateProjectFlow {
    /// The local device id is retained internally for project compatibility;
    /// it is not a picker choice or a visible concept in this flow.
    device: Option<Device>,
    /// Filter input; the right arrow descends into the highlighted folder and
    /// Enter chooses the folder currently shown in the breadcrumbs.
    search: Entity<ComposerInput>,
    browser: Loadable<FolderListing>,
    /// Requested browser path (`None` = the local device's home).
    browser_path: Option<String>,
    /// The local home (the path a `None` browse resolved to) — breadcrumbs fold
    /// everything up to here into the Home crumb.
    home: Option<String>,
    /// Best-effort git seed for the CURRENT browser path (known when we
    /// descended through an entry whose `is_repo` we saw; the owning device's
    /// ProjectsSync re-verifies either way).
    browser_repo: bool,
    /// Keyboard highlight within the FILTERED folder rows.
    active: usize,
    submit_busy: bool,
    error: Option<SharedString>,
    /// Tracked on the card (`track_focus`) — puts the card on the keyboard
    /// dispatch path so ↑↓/⌫/esc reach `add_project_key` while the search input
    /// holds focus (the structure every working picker uses).
    focus: FocusHandle,
    /// Folder-list scroll — keyboard navigation keeps the highlighted row in
    /// view (`scroll_to_item`).
    list_scroll: gpui::ScrollHandle,
    focus_pending: bool,
    load_task: Option<Task<()>>,
    submit_task: Option<Task<()>>,
    _search_events: Subscription,
}

/// The project-row Rename dialog (same shape as [`RenameChatDialog`]).
pub(super) struct RenameProjectDialog {
    pub project_id: String,
    pub input: Entity<ComposerInput>,
    pub focus_pending: bool,
    pub _events: Subscription,
}

/// Dot color for a chat's display status (tab dots + Sessions rows).
pub(super) fn status_dot_color(status: ChatIndicator, theme: &Theme) -> gpui::Hsla {
    match status {
        // Pink, not amber — the harsh yellow read as a warning; running is
        // routine (user request).
        ChatIndicator::Working => {
            crate::theme::oklch(0.718, 0.202, 349.761).opacity(0.85) // pink-400
        }
        // Blue: "asking you a question" must read differently from "busy
        // working" at a glance.
        ChatIndicator::AwaitingInput => theme.accent.opacity(0.9),
        ChatIndicator::Errored => theme.danger,
        // Finished and idle are both quiet neutral dots.
        ChatIndicator::Completed => crate::theme::white_alpha(0.16),
        ChatIndicator::Idle => crate::theme::white_alpha(0.14),
    }
}

impl Shell {
    // ---- project switching ----

    /// Land in a project: remembered tab if alive, else the most recent chat in
    /// the project, else the new-session canvas. Persists `last_project_id`.
    pub(super) fn activate_project(&mut self, project_id: String, cx: &mut Context<Self>) {
        self.route = Route::Chat;
        self.state.update(cx, |s, cx| {
            s.select_project(Some(project_id.clone()), cx);
        });
        let target = {
            let state = self.state.read(cx);
            let in_project = |id: &str| {
                state
                    .visible_chats()
                    .any(|c| c.id == id && c.project_id.as_deref() == Some(project_id.as_str()))
            };
            self.project_last_chat
                .get(&project_id)
                .filter(|id| in_project(id))
                .cloned()
                .or_else(|| {
                    // `visible_chats` is recency-sorted — first match is the
                    // most recent chat of the project.
                    state
                        .visible_chats()
                        .find(|c| c.project_id.as_deref() == Some(project_id.as_str()))
                        .map(|c| c.id.clone())
                })
        };
        self.state.update(cx, |s, cx| s.select_chat(target, cx));
        self.settings.last_project_id = Some(project_id);
        self.schedule_save(cx);
        cx.notify();
    }

    /// Open a fresh session canvas in a project. The chat is materialized when
    /// the user sends its first prompt, so clicking a sidebar affordance never
    /// leaves behind empty sessions.
    pub(super) fn open_new_session(&mut self, project_id: Option<String>, cx: &mut Context<Self>) {
        let project_id = project_id.or_else(|| self.state.read(cx).selected_project.clone());
        let Some(project_id) = project_id else {
            // A session needs a project; keep the empty-state path useful.
            self.open_add_project(cx);
            return;
        };
        self.route = Route::Chat;
        self.state.update(cx, |s, cx| {
            s.select_project(Some(project_id), cx);
            s.select_chat(None, cx);
        });
        cx.notify();
    }

    /// Create the first session for a folder and land on it. The pi child is
    /// spawned when the user sends the first prompt; creating the chat here
    /// makes folder selection feel like one onboarding action instead of two.
    fn create_session_in_project(&mut self, project_id: String, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.sidebar_notice = Some("Engine not connected".into());
            cx.notify();
            return;
        };
        let chat_id = uuid::Uuid::new_v4().to_string();
        let submit_id = chat_id.clone();
        let params = serde_json::json!({
            "op": "createChat",
            "chatId": chat_id,
            "projectId": project_id,
        });
        self.mutate_task = Some(cx.spawn(async move |this, cx| {
            let result = engine.client().call(methods::MUTATE, params).await;
            this.update(cx, |shell, cx| {
                match result {
                    Ok(_) => {
                        shell.state.update(cx, |state, cx| {
                            state.select_chat(Some(submit_id.clone()), cx);
                        });
                    }
                    Err(err) => {
                        shell.sidebar_notice =
                            Some(format!("Couldn't create the session: {err}").into());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    // ---- sidebar sections ----

    /// The "Projects" section: tracked header + add button, then a row per project.
    pub(super) fn render_projects_section(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // A drag that ended off-list (no drop event) must not strand the
        // sibling slide offsets.
        if self.project_drag.is_some() && !cx.has_active_drag() {
            self.project_drag = None;
        }
        let (projects, selected, attention): (
            Vec<Project>,
            Option<String>,
            std::collections::HashMap<String, ChatIndicator>,
        ) = {
            let now = Utc::now();
            let state = self.state.read(cx);
            let projects = state.projects.clone();
            // Projects with a live/awaiting session get an aggregate dot (the
            // most urgent member status wins) so the attention signal survives
            // even with the Sessions list scrolled off.
            let mut attention: std::collections::HashMap<String, ChatIndicator> =
                std::collections::HashMap::new();
            for chat in state.visible_chats() {
                let status = state.display_status_for(chat, now);
                if !matches!(
                    status,
                    ChatIndicator::Working | ChatIndicator::AwaitingInput
                ) {
                    continue;
                }
                let Some(project_id) = chat.project_id.clone() else {
                    continue;
                };
                attention
                    .entry(project_id)
                    .and_modify(|held| {
                        if crate::state::attention_rank(status)
                            < crate::state::attention_rank(*held)
                        {
                            *held = status;
                        }
                    })
                    .or_insert(status);
            }
            (projects, state.selected_project.clone(), attention)
        };
        // Manual (drag) order overrides the synced creation order — device-
        // local, resolved exactly like the session-tab order.
        let projects: Vec<Project> = {
            let created: Vec<String> = projects.iter().map(|s| s.id.clone()).collect();
            let order = super::tabs::resolve_tab_order(&created, &self.settings.project_order);
            let mut by_id: std::collections::HashMap<String, Project> =
                projects.into_iter().map(|s| (s.id.clone(), s)).collect();
            order.iter().filter_map(|id| by_id.remove(id)).collect()
        };

        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .px(px(Theme::SPACE_SM))
            .pt(px(8.0))
            .pb(px(4.0))
            .child(
                div()
                    .text_size(px(11.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text_muted.opacity(0.6))
                    .child(SharedString::from("Projects")),
            )
            .child(
                div()
                    .id("add-project")
                    .size(px(20.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(5.0))
                    .cursor_pointer()
                    .bg(motion::hover_blend(
                        "add-project",
                        crate::theme::wash(0.0),
                        crate::theme::wash(0.14),
                    ))
                    .on_hover(motion::hover_listener("add-project"))
                    .on_click(cx.listener(|this, _, _, cx| this.open_add_project(cx)))
                    .child(
                        icon(icons::PLUS)
                            .size(px(14.0))
                            .text_color(theme.text_muted.opacity(0.7)),
                    ),
            );

        let mut column = div().flex().flex_col().child(header);
        if projects.is_empty() {
            // Ghost row: the empty-state affordance mirrors a project row.
            column = column.child(
                div()
                    .id("add-project-ghost")
                    .mx(px(0.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(Theme::SPACE_SM))
                    .rounded(px(8.0))
                    .px(px(Theme::SPACE_SM))
                    .py(px(6.0))
                    .text_size(px(13.0))
                    .text_color(motion::hover_blend(
                        "add-project-ghost",
                        theme.text_muted,
                        theme.text,
                    ))
                    .bg(motion::hover_blend(
                        "add-project-ghost",
                        crate::theme::wash(0.0),
                        theme.element_hover,
                    ))
                    .on_hover(motion::hover_listener("add-project-ghost"))
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| this.open_add_project(cx)))
                    .child(
                        icon(icons::FOLDER)
                            .size(px(16.0))
                            .text_color(theme.text_muted),
                    )
                    .child(SharedString::from("Create project")),
            );
        } else {
            let count = projects.len();
            let drag = self
                .project_drag
                .as_ref()
                .map(|d| (d.from, d.over, d.epoch, d.prev_over));
            let rows: Vec<AnyElement> = projects
                .into_iter()
                .enumerate()
                .map(|(ix, project)| {
                    let id = project.id.clone();
                    let is_selected = selected.as_deref() == Some(project.id.as_str());
                    let attention = attention.get(&project.id).copied();
                    let row =
                        self.render_project_row(ix, project, is_selected, attention, theme, cx);
                    // Sliding transform while a sibling is dragged over —
                    // the session-tab idiom, vertical.
                    match drag {
                        Some((from, over, epoch, prev_over)) if ix != from => {
                            let target = slide_offset(ix, from, over) * PROJECT_ROW_SLOT;
                            let start = slide_offset(ix, from, prev_over) * PROJECT_ROW_SLOT;
                            div()
                                .relative()
                                .child(row.with_animation(
                                    SharedString::from(format!("project-slide-{id}-{epoch}")),
                                    TAB_SLIDE.animation(),
                                    move |el, t| el.top(px(motion::lerp(start, target, t))),
                                ))
                                .into_any_element()
                        }
                        // The dragged row renders as an invisible spacer; the
                        // cursor ghost represents it.
                        Some((from, ..)) if ix == from => div()
                            .h(px(PROJECT_ROW_SLOT - 2.0))
                            .flex_none()
                            .into_any_element(),
                        _ => row.into_any_element(),
                    }
                })
                .collect();
            column = column.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .on_drag_move::<ProjectDragPayload>(cx.listener(
                        move |this, event: &gpui::DragMoveEvent<ProjectDragPayload>, _, cx| {
                            let from = event.drag(cx).from;
                            let rel_y =
                                f32::from(event.event.position.y) - f32::from(event.bounds.top());
                            let over = drop_index(rel_y, PROJECT_ROW_SLOT, count);
                            this.update_project_drag_over(from, over, cx);
                        },
                    ))
                    .on_drop::<ProjectDragPayload>(cx.listener(
                        move |this, payload: &ProjectDragPayload, _, cx| {
                            let to = this
                                .project_drag
                                .as_ref()
                                .map(|d| d.over)
                                .unwrap_or(payload.from);
                            this.commit_project_reorder(payload.from, to, cx);
                        },
                    ))
                    .children(rows),
            );
        }
        column.into_any_element()
    }

    /// Track the drop slot while a project row is dragged over the list (150ms
    /// sibling slides restart per committed `over` change).
    fn update_project_drag_over(&mut self, from: usize, over: usize, cx: &mut Context<Self>) {
        match &mut self.project_drag {
            Some(drag) if drag.from == from => {
                if drag.over != over {
                    drag.prev_over = drag.over;
                    drag.over = over;
                    drag.epoch += 1;
                    cx.notify();
                }
            }
            _ => {
                self.project_drag = Some(ProjectDragState {
                    from,
                    over,
                    epoch: 0,
                    prev_over: from,
                });
                cx.notify();
            }
        }
    }

    /// Commit a drag: persist the new visual order (device-local).
    fn commit_project_reorder(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        let created: Vec<String> = self
            .state
            .read(cx)
            .projects
            .iter()
            .map(|s| s.id.clone())
            .collect();
        let mut order = super::tabs::resolve_tab_order(&created, &self.settings.project_order);
        if from < order.len() {
            reorder_tabs(&mut order, from, to);
            self.settings.project_order = order;
            self.schedule_save(cx);
        }
        self.project_drag = None;
        cx.notify();
    }

    /// One project row: folder icon + folder name.
    #[allow(clippy::too_many_arguments)]
    fn render_project_row(
        &self,
        ix: usize,
        project: Project,
        selected: bool,
        attention: Option<ChatIndicator>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let id = project.id.clone();
        let name: SharedString = project.display_name().to_string().into();
        let fade_key = format!("project-row-{id}");
        let rest_bg = if selected {
            crate::theme::glass_selected_bg()
        } else {
            crate::theme::wash(0.0)
        };
        let rest_text = if selected {
            theme.text
        } else {
            theme.text.opacity(0.8)
        };
        let select_id = id.clone();
        let menu_id = id.clone();
        let new_session_id = id.clone();
        let group = SharedString::from(format!("project-row-group-{id}"));
        div()
            .id(SharedString::from(format!("project-{id}")))
            .group(group.clone())
            .flex()
            .flex_row()
            .items_center()
            .gap(px(Theme::SPACE_SM))
            .rounded(px(8.0))
            .px(px(Theme::SPACE_SM))
            .py(px(6.0))
            .text_color(motion::hover_blend(&fade_key, rest_text, theme.text))
            .bg(motion::hover_blend(&fade_key, rest_bg, theme.element_hover))
            .when(selected, |el| {
                el.shadow(crate::theme::glass_selected_shadows())
            })
            .on_hover(motion::hover_listener(fade_key))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                this.activate_project(select_id.clone(), cx);
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    this.project_menu = Some((menu_id.clone(), event.position));
                    cx.notify();
                }),
            )
            .on_drag(
                ProjectDragPayload {
                    from: ix,
                    name: name.clone(),
                },
                |payload, _point, _, cx| {
                    let name = payload.name.clone();
                    cx.stop_propagation();
                    cx.new(|_| ProjectGhost { name })
                },
            )
            // Status dot LEADS the row (like session rows) so its position is
            // stable — appearing/disappearing at the right edge made the row
            // jitter (user request). Faint at rest, colored under attention.
            .child(
                div().size(px(6.0)).rounded_full().flex_none().bg(attention
                    .map(|status| status_dot_color(status, theme))
                    .unwrap_or_else(|| crate::theme::white_alpha(0.14))),
            )
            .child(
                icon(icons::FOLDER)
                    .size(px(16.0))
                    .flex_none()
                    .text_color(theme.text_muted),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(13.0))
                    .line_height(px(17.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .child(name),
            )
            .child(
                div()
                    .id(SharedString::from(format!("new-session-project-{id}")))
                    .size(px(20.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(5.0))
                    .cursor_pointer()
                    .opacity(0.0)
                    .group_hover(group, |el| el.opacity(1.0))
                    .hover(|el| el.bg(crate::theme::wash(0.14)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.open_new_session(Some(new_session_id.clone()), cx);
                    }))
                    .child(
                        icon(icons::PLUS)
                            .size(px(14.0))
                            .text_color(theme.text_muted.opacity(0.75)),
                    ),
            )
    }

    /// The global "Sessions" list: every session across all projects (idle
    /// included), attention-sorted. Rows are keyed for the FLIP resort glide.
    pub(super) fn render_active_rows(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Vec<(String, f32, AnyElement)> {
        let now = Utc::now();
        let rows: Vec<(ChatIndicator, comet_proto::Chat, String, Option<String>)> = {
            let state = self.state.read(cx);
            state
                .overview_chats(now)
                .into_iter()
                .map(|(status, chat)| {
                    let project = state.project_for_chat(chat);
                    let folder = project
                        .map(|s| s.display_name().to_string())
                        .unwrap_or_else(|| "?".to_string());
                    // The branch shows whenever the engine has stamped one —
                    // main-checkout sessions included, not just worktrees.
                    let branch = chat
                        .branch
                        .as_deref()
                        .map(str::trim)
                        .filter(|b| !b.is_empty())
                        .map(str::to_string);
                    (status, chat.clone(), folder, branch)
                })
                .collect()
        };
        let selected = self.state.read(cx).selected_chat.clone();
        rows.into_iter()
            .map(|(status, chat, folder, branch)| {
                let time_ago: SharedString =
                    format_time_ago(chat.last_message_at.unwrap_or(chat.created_at), now).into();
                let is_selected = selected.as_deref() == Some(chat.id.as_str());
                // Rows with a command line are one 16px line taller.
                let height = super::CHAT_ROW_HEIGHT
                    + if chat.last_command.is_some() {
                        super::CHAT_ROW_COMMAND_LINE
                    } else {
                        0.0
                    };
                let harness = chat.config.as_ref().map(|c| c.harness);
                let element = self.render_chat_row(
                    chat.id.clone(),
                    transcript::single_line(
                        &chat.title.clone().unwrap_or_else(|| "New session".into()),
                    )
                    .into(),
                    time_ago,
                    folder.into(),
                    branch.map(SharedString::from),
                    chat.last_command.clone().map(SharedString::from),
                    harness,
                    status,
                    is_selected,
                    theme,
                    cx,
                );
                (format!("c:{}", chat.id), height, element)
            })
            .collect()
    }

    // ---- add-project flow (the ⌘K palette) ----

    pub(super) fn open_add_project(&mut self, cx: &mut Context<Self>) {
        let state = self.state.read(cx);
        let device = state
            .local_device_id
            .as_deref()
            .and_then(|local_id| state.devices.iter().find(|d| d.id == local_id))
            .cloned();
        // "PaletteSearch" context: navigation keys stay unbound so ↑↓/←/→/⏎
        // bubble to the palette frame (`add_project_key`) instead of moving the
        // text caret — Enter and ⌘Enter are both handled there.
        let search =
            cx.new(|cx| ComposerInput::with_context("Search folders…", "PaletteSearch", cx));
        let search_events = cx.subscribe(&search, |this: &mut Shell, _, event, cx| {
            if matches!(event, ComposerInputEvent::Edited) {
                if let Some(flow) = this.add_project.as_mut() {
                    flow.active = 0;
                }
                cx.notify();
            }
        });
        let has_device = device.is_some();
        self.add_project = Some(CreateProjectFlow {
            device,
            search,
            browser: Loadable::Idle,
            browser_path: None,
            home: None,
            browser_repo: false,
            active: 0,
            submit_busy: false,
            error: None,
            focus: cx.focus_handle(),
            list_scroll: gpui::ScrollHandle::new(),
            focus_pending: true,
            load_task: None,
            submit_task: None,
            _search_events: search_events,
        });
        if has_device {
            self.load_project_folders(None, cx);
        }
        cx.notify();
    }

    /// The current listing's folder rows filtered by the search query
    /// (prefix matches first — `popover::filter_indices`).
    fn add_project_filtered(&self, cx: &App) -> Vec<comet_proto::FolderEntry> {
        let Some(flow) = self.add_project.as_ref() else {
            return Vec::new();
        };
        let Some(listing) = flow.browser.ready() else {
            return Vec::new();
        };
        let dirs = browser_rows(listing);
        let query = flow.search.read(cx).text().to_string();
        let names: Vec<&str> = dirs.iter().map(|e| e.name.as_str()).collect();
        popover::filter_indices(&query, &names)
            .into_iter()
            .map(|ix| dirs[ix].clone())
            .collect()
    }

    /// Descend into the highlighted (filtered) folder; clears the query.
    fn add_project_open_active(&mut self, cx: &mut Context<Self>) {
        let rows = self.add_project_filtered(cx);
        let Some(flow) = self.add_project.as_ref() else {
            return;
        };
        let Some(listing) = flow.browser.ready() else {
            return;
        };
        let Some(entry) = rows.get(flow.active) else {
            return;
        };
        let full = crate::pickers::child_path(&listing.path, &entry.name);
        let is_repo = entry.is_repo;
        let search = flow.search.clone();
        if let Some(flow) = self.add_project.as_mut() {
            flow.browser_repo = is_repo;
        }
        search.update(cx, |input, cx| input.set_text("", cx));
        self.load_project_folders(Some(full), cx);
    }

    /// Descend into a specific folder row (mouse path); clears the query.
    fn add_project_descend(&mut self, full: String, is_repo: bool, cx: &mut Context<Self>) {
        let Some(flow) = self.add_project.as_mut() else {
            return;
        };
        flow.browser_repo = is_repo;
        let search = flow.search.clone();
        search.update(cx, |input, cx| input.set_text("", cx));
        self.load_project_folders(Some(full), cx);
    }

    /// ListFolders on the local engine.
    pub(super) fn load_project_folders(&mut self, path: Option<String>, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(flow) = self.add_project.as_mut() else {
            return;
        };
        let went_home = path.is_none();
        flow.browser_path = path.clone();
        flow.browser = Loadable::Loading;
        flow.active = 0;
        flow.list_scroll.set_offset(gpui::Point::default());
        flow.load_task = Some(cx.spawn(async move |this, cx| {
            let mut params = serde_json::Map::new();
            if let Some(p) = &path {
                params.insert("path".into(), serde_json::Value::String(p.clone()));
            }
            let result = engine
                .client()
                .call(methods::LIST_FOLDERS, serde_json::Value::Object(params))
                .await;
            this.update(cx, |shell, cx| {
                if let Some(flow) = shell.add_project.as_mut() {
                    flow.browser = match result {
                        Ok(value) => match serde_json::from_value::<FolderListing>(value) {
                            Ok(listing) => {
                                // A pathless browse resolved home — remember it
                                // so the breadcrumbs can fold it into the
                                // Home crumb.
                                if went_home {
                                    flow.home = Some(listing.path.clone());
                                }
                                Loadable::Ready(listing)
                            }
                            Err(err) => Loadable::Error(err.to_string()),
                        },
                        Err(err) => Loadable::Error(err.to_string()),
                    };
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Create the project for the browser's current folder.
    fn submit_add_project(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(flow) = self.add_project.as_ref() else {
            return;
        };
        if flow.submit_busy {
            return;
        }
        let Some(device) = flow.device.clone() else {
            return;
        };
        let Some(listing) = flow.browser.ready() else {
            return;
        };
        let path = listing.path.clone();
        let git_detected = flow.browser_repo;
        // Same (device, folder) already has a project → just switch to it. The
        // engine dedupes this case too (a createProject for a duplicate pair
        // no-ops), so creating would leave the minted id dangling.
        if let Some(existing) = self
            .state
            .read(cx)
            .projects
            .iter()
            .find(|s| s.device_id == device.id && s.path == path)
            .map(|s| s.id.clone())
        {
            self.add_project = None;
            self.activate_project(existing.clone(), cx);
            self.create_session_in_project(existing, cx);
            return;
        }
        let Some(flow) = self.add_project.as_mut() else {
            return;
        };
        flow.submit_busy = true;
        flow.error = None;
        let project_id = uuid::Uuid::new_v4().to_string();
        // Optimistic echo: the watch frame carrying the real row replaces it
        // by id (apply_projects re-sorts; same-id upsert is idempotent).
        let project = Project {
            id: project_id.clone(),
            device_id: device.id.clone(),
            path: path.clone(),
            name: None,
            git_detected,
            git_checked_at: None,
            checkout_id: None,
            created_at: Utc::now(),
        };
        self.state.update(cx, |s, cx| {
            if !s.projects.iter().any(|existing| existing.id == project.id) {
                s.projects.push(project);
            }
            cx.notify();
        });
        let params = serde_json::json!({
            "op": "createProject",
            "projectId": project_id,
            "deviceId": device.id,
            "path": path,
            "gitDetected": git_detected,
        });
        let submit_id = project_id.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = engine.client().call(methods::MUTATE, params).await;
            this.update(cx, |shell, cx| {
                match result {
                    Ok(_) => {
                        shell.add_project = None;
                        shell.activate_project(submit_id.clone(), cx);
                        shell.create_session_in_project(submit_id.clone(), cx);
                    }
                    Err(err) => {
                        // Roll the optimistic row back; surface the error inline.
                        shell.state.update(cx, |s, cx| {
                            s.projects.retain(|project| project.id != submit_id);
                            cx.notify();
                        });
                        if let Some(flow) = shell.add_project.as_mut() {
                            flow.submit_busy = false;
                            flow.error = Some(format!("{err}").into());
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        });
        if let Some(flow) = self.add_project.as_mut() {
            flow.submit_task = Some(task);
        }
        cx.notify();
    }

    /// Go up to the parent folder (←, and ⌫ on an empty query).
    fn add_project_go_up(&mut self, cx: &mut Context<Self>) {
        let parent = self
            .add_project
            .as_ref()
            .and_then(|f| f.browser.ready())
            .and_then(|l| parent_path(&l.path));
        if let Some(parent) = parent {
            if let Some(flow) = self.add_project.as_mut() {
                flow.browser_repo = false; // unknown at the parent
            }
            self.load_project_folders(Some(parent), cx);
        }
    }

    /// Palette keys (bubbling from the focused search input) — every legend
    /// maps to a REAL key: ↑↓ navigate, → open the highlighted folder,
    /// ← up a level, ⏎ or ⌘⏎ add the folder shown in the breadcrumbs,
    /// ⌫ (empty query) also goes up, esc closes.
    fn add_project_key(&mut self, event: &gpui::KeyDownEvent, cx: &mut Context<Self>) {
        // ←/→ act on the FOLDERS, not the text cursor — the palette is a
        // navigator first; queries are short and edited with ⌫.
        match event.keystroke.key.as_str() {
            "right" => {
                self.add_project_open_active(cx);
                return;
            }
            "left" => {
                self.add_project_go_up(cx);
                return;
            }
            _ => {}
        }
        let key = popover::classify_key(
            event.keystroke.key.as_str(),
            event.keystroke.modifiers.platform,
            event.keystroke.modifiers.control,
        );
        match key {
            popover::MenuKey::Escape => {
                self.add_project = None;
                cx.notify();
            }
            popover::MenuKey::Up | popover::MenuKey::Down => {
                let count = self.add_project_filtered(cx).len();
                let delta = if key == popover::MenuKey::Up { -1 } else { 1 };
                if let Some(flow) = self.add_project.as_mut() {
                    flow.active = popover::menu_step(Some(flow.active), count, delta).unwrap_or(0);
                    // Keep the highlighted row in view as the cursor walks
                    // past the viewport (user-reported: the list didn't
                    // follow the keyboard).
                    flow.list_scroll.scroll_to_item(flow.active);
                    cx.notify();
                }
            }
            // Enter chooses the folder open in the breadcrumbs. Use → when
            // the intent is to descend into the highlighted subfolder.
            popover::MenuKey::Enter => self.submit_add_project(cx),
            popover::MenuKey::ModEnter => self.submit_add_project(cx),
            popover::MenuKey::Backspace => {
                let empty = self
                    .add_project
                    .as_ref()
                    .is_some_and(|f| f.search.read(cx).is_empty());
                if empty {
                    self.add_project_go_up(cx);
                }
            }
            popover::MenuKey::Other => {}
        }
    }

    /// The palette card: ⌘K search bar (with the ⌘⏎ add / esc chips), local
    /// breadcrumbs, folder list, and kbd-hint footer.
    pub(super) fn render_add_project_overlay(
        &mut self,
        viewport: gpui::Size<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let theme = Theme::of(cx).clone();
        {
            let flow = self.add_project.as_mut()?;
            if std::mem::take(&mut flow.focus_pending) {
                let handle = flow.search.focus_handle(cx);
                window.focus(&handle, cx);
            }
        }
        let (
            search,
            error,
            submit_busy,
            active,
            loading,
            load_error,
            listing,
            focus,
            list_scroll,
            home,
        ) = {
            let flow = self.add_project.as_ref()?;
            (
                flow.search.clone(),
                flow.error.clone(),
                flow.submit_busy,
                flow.active,
                matches!(flow.browser, Loadable::Loading | Loadable::Idle),
                flow.browser.error().map(str::to_string),
                flow.browser.ready().cloned(),
                flow.focus.clone(),
                flow.list_scroll.clone(),
                flow.home.clone(),
            )
        };
        let rows = self.add_project_filtered(cx);
        let query_empty = search.read(cx).is_empty();
        let hairline = crate::theme::white_alpha(0.06);

        // A quiet mono key-cap chip ("⌘K" / "esc") for the search bar ends.
        let key_chip = |theme: &Theme| {
            div()
                .h(px(22.0))
                .px(px(6.0))
                .rounded(px(5.0))
                .flex_none()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(2.0))
                .bg(crate::theme::white_alpha(0.05))
                .text_size(px(11.0))
                .font_family(theme.font_mono.clone())
                .text_color(theme.text_muted.opacity(0.7))
        };

        // ── search bar (the ⌘K bar): summon chip · input · use-this-folder
        //    action · esc. Keep the shortcut visible, but name the action so
        //    the current folder is easy to choose without knowing the chord.
        let submit_chip = popover::btn_primary(&theme, "")
            .id("add-project-submit")
            .h(px(22.0))
            .px(px(8.0))
            .py(px(0.0))
            // Match the key-cap chips beside it (rounded-5) — btn_primary's
            // rounded-8 at this size read as a different component.
            .rounded(px(5.0))
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .text_size(px(12.0))
            .when(submit_busy || listing.is_none(), |el| el.opacity(0.6))
            .on_click(cx.listener(|this, _, _, cx| this.submit_add_project(cx)))
            .when(!submit_busy, |el| {
                el.child(SharedString::from("use this folder")).child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(2.0))
                        .text_color(theme.bg.opacity(0.72))
                        .child(icon(icons::COMMAND).size(px(11.0)))
                        .child(SharedString::from("enter")),
                )
            })
            .when(submit_busy, |el| el.child(SharedString::from("Adding…")));
        // Header and footer sit a shade DEEPER than the body (the shared
        // recessed-band tone) — the bands frame the folder list, which stays
        // on the brighter tint.
        let band = popover::band();
        let input_row = div()
            .h(px(46.0))
            .flex_none()
            .pl(px(12.0))
            .pr(px(10.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .bg(band)
            .border_b_1()
            .border_color(hairline)
            .child(
                key_chip(&theme)
                    .child(
                        icon(icons::COMMAND)
                            .size(px(11.0))
                            .text_color(theme.text_muted.opacity(0.7)),
                    )
                    .child(SharedString::from("K")),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(14.0))
                    .child(search.clone().into_any_element()),
            )
            .child(submit_chip)
            .child(
                key_chip(&theme)
                    .id("add-project-esc")
                    .cursor_pointer()
                    .hover(|s| s.bg(crate::theme::white_alpha(0.09)))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.add_project = None;
                        cx.notify();
                    }))
                    .child(SharedString::from("esc")),
            );

        // ── breadcrumbs ("Home / Projects / nova"): the quiet mono path
        //    voice, `/` separators. The Home crumb stands in for the local
        //    home path; below home the full path shows. Ancestors are
        //    clickable.
        let crumbs: AnyElement = match &listing {
            Some(listing) => {
                let segments = breadcrumbs(&listing.path);
                let last = segments.len().saturating_sub(1);
                // Root "/" chip always folds; the home segments fold too when
                // the browsed path sits at/under home.
                let at_home = home.as_deref() == Some(listing.path.as_str());
                let folded = 1 + home
                    .as_deref()
                    .filter(|h| listing.path == *h || listing.path.starts_with(&format!("{h}/")))
                    .map(|h| h.split('/').filter(|s| !s.is_empty()).count())
                    .unwrap_or(0);
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .px(px(13.0))
                    .pt(px(10.0))
                    .pb(px(2.0))
                    .text_size(px(11.0))
                    .font_family(theme.font_mono.clone())
                    .child({
                        let crumb = div()
                            .id("add-project-crumb-home")
                            .px(px(3.0))
                            .rounded(px(4.0))
                            .child(SharedString::from("Home"));
                        if at_home {
                            // Standing at home — the Home crumb IS the
                            // current folder.
                            crumb
                                .text_color(theme.text.opacity(0.85))
                                .into_any_element()
                        } else {
                            crumb
                                .text_color(theme.text_muted.opacity(0.55))
                                .cursor_pointer()
                                .hover(|s| s.text_color(theme.text))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(flow) = this.add_project.as_mut() {
                                        flow.browser_repo = false;
                                    }
                                    this.load_project_folders(None, cx);
                                }))
                                .into_any_element()
                        }
                    })
                    .children(segments.into_iter().enumerate().skip(folded).map(
                        |(ix, (label, full))| {
                            let is_last = ix == last;
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .child(
                                    div()
                                        .text_color(theme.text_faint.opacity(0.7))
                                        .child(SharedString::from("/")),
                                )
                                .child({
                                    let crumb = div()
                                        .id(("add-project-crumb", ix))
                                        .px(px(3.0))
                                        .rounded(px(4.0))
                                        .text_color(if is_last {
                                            theme.text.opacity(0.85)
                                        } else {
                                            theme.text_muted.opacity(0.55)
                                        })
                                        .child(SharedString::from(label));
                                    if is_last {
                                        crumb.into_any_element()
                                    } else {
                                        crumb
                                            .cursor_pointer()
                                            .hover(|s| s.text_color(theme.text))
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                if let Some(flow) = this.add_project.as_mut() {
                                                    flow.browser_repo = false;
                                                }
                                                this.load_project_folders(Some(full.clone()), cx);
                                            }))
                                            .into_any_element()
                                    }
                                })
                        },
                    ))
                    .into_any_element()
            }
            None => div().pt(px(6.0)).into_any_element(),
        };

        // ── folder list ─────────────────────────────────────────────────────
        let base_path = listing.as_ref().map(|l| l.path.clone()).unwrap_or_default();
        let list: AnyElement = if loading {
            div()
                .px(px(8.0))
                .py(px(6.0))
                .child(popover::skeleton_rows("add-project-skeleton", &theme, 6))
                .into_any_element()
        } else if let Some(message) = load_error {
            popover::error_row(&theme, &message)
                .px(px(14.0))
                .py(px(10.0))
                .child(
                    div()
                        .id("add-project-retry")
                        .px(px(Theme::SPACE_SM))
                        .py(px(3.0))
                        .rounded(px(Theme::CONTROL_RADIUS))
                        .border_1()
                        .border_color(theme.border)
                        .text_color(theme.text)
                        .cursor_pointer()
                        .hover(|s| s.bg(theme.element_hover))
                        .on_click(cx.listener(|this, _, _, cx| {
                            let path = this
                                .add_project
                                .as_ref()
                                .and_then(|f| f.browser_path.clone());
                            this.load_project_folders(path, cx);
                        }))
                        .child(SharedString::from("Retry")),
                )
                .into_any_element()
        } else if rows.is_empty() {
            div()
                .px(px(14.0))
                .py(px(16.0))
                .text_size(px(12.5))
                .text_color(theme.text_faint)
                .child(SharedString::from(if query_empty {
                    "No folders here"
                } else {
                    "No folders match"
                }))
                .into_any_element()
        } else {
            // The 6px gutters live on a WRAPPER, outside the scroll viewport:
            // in-content padding/spacers can't do it — the wheel's max offset
            // eats bottom padding, and `scroll_to_item` (keyboard) pins the
            // row's bottom to the viewport edge regardless.
            div()
                .flex_1()
                .min_h_0()
                .py(px(6.0))
                .child(
                    div()
                        .id("add-project-folders")
                        .size_full()
                        .overflow_y_scroll()
                        .track_scroll(&list_scroll)
                        .px(px(8.0))
                        .flex()
                        .flex_col()
                        // The app-wide list rhythm (sidebar rows, menu rows): 2px.
                        .gap(px(2.0))
                        .children(rows.into_iter().enumerate().map(|(ix, entry)| {
                            let name: SharedString = entry.name.clone().into();
                            let full = crate::pickers::child_path(&base_path, &entry.name);
                            let is_repo = entry.is_repo;
                            popover::menu_row_nav(
                                &theme,
                                false,
                                ix == active,
                                format!("add-project-folder-{ix}"),
                            )
                            // The active-tab/session selection language: the wash
                            // plus the ring-only inset outline.
                            .when(ix == active, |el| {
                                el.shadow(crate::theme::glass_selected_shadows())
                            })
                            .id(("add-project-folder", ix))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.add_project_descend(full.clone(), is_repo, cx);
                            }))
                            .child(
                                icon(icons::FOLDER)
                                    .size(px(15.0))
                                    .flex_none()
                                    .text_color(theme.text_muted.opacity(0.8)),
                            )
                            .child(div().flex_1().min_w_0().truncate().child(name))
                            // Repos get a quiet trailing branch glyph — the row
                            // you're usually hunting for announces itself.
                            .when(is_repo, |el| {
                                el.child(
                                    icon(icons::GIT_BRANCH)
                                        .size(px(13.0))
                                        .flex_none()
                                        .text_color(theme.text_muted.opacity(0.5)),
                                )
                            })
                        })),
                )
                .into_any_element()
        };

        // ── body: local folder column (crumbs + list).
        //    FIXED height — sparse folders and loading skeletons must not
        //    resize the card (the list fills and scrolls).
        let body = div().h(px(330.0)).flex().flex_row().items_stretch().child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(crumbs)
                .child(list),
        );

        // ── footer: the shared key-cap legend voice (popover::key_hint).
        let footer = div()
            .flex_none()
            .bg(band)
            .border_t_1()
            .border_color(hairline)
            .px(px(12.0))
            .py(px(8.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(12.0))
            .child(popover::key_hint_pair(
                &theme,
                icons::ARROW_UP,
                icons::ARROW_DOWN,
                "Navigate",
            ))
            .child(popover::key_hint(&theme, icons::ARROW_LEFT, "Up"))
            .child(popover::key_hint(&theme, icons::ARROW_RIGHT, "Open"))
            .child(popover::key_hint(&theme, icons::RETURN, "use this folder"))
            .when_some(error, |el, message| {
                el.child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(px(11.0))
                        .text_color(theme.danger)
                        .child(message),
                )
            });

        let card = div()
            .id("add-project-palette")
            .w(px(680.0))
            .rounded(px(14.0))
            .border_1()
            .border_color(crate::theme::white_alpha(0.10))
            // The popover_card glass recipe: a translucent tint over the
            // frosted backdrop blur (`popover::modal` wraps in `frosted`) —
            // an opaque fill here killed the vibrancy every other float has.
            .bg(theme.glass_card())
            .shadow_lg()
            .overflow_hidden()
            .flex()
            .flex_col()
            .text_color(theme.text)
            // On the keyboard dispatch path (see `CreateProjectFlow::focus`) — the
            // pickers' proven structure for frame-level keys with a focused
            // child input.
            .track_focus(&focus)
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                this.add_project_key(event, cx)
            }))
            // Clicking the scrim dismisses (user requirement) — same close
            // path as Escape.
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.add_project = None;
                cx.notify();
            }))
            .child(input_row)
            .child(body)
            .child(footer)
            .into_any_element();
        Some(popover::modal("add-project-dialog", viewport, card))
    }

    // ---- project context menu / rename / delete overlays ----

    pub(super) fn open_rename_project(&mut self, project_id: String, cx: &mut Context<Self>) {
        self.project_menu = None;
        let current = self
            .state
            .read(cx)
            .project_row(&project_id)
            .map(|s| s.display_name().to_string())
            .unwrap_or_default();
        let input = cx.new(|cx| ComposerInput::new("Project name", cx));
        input.update(cx, |input, cx| input.set_text(current, cx));
        let events = cx.subscribe(&input, |this: &mut Shell, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.submit_rename_project(cx);
            }
        });
        self.rename_project_dialog = Some(RenameProjectDialog {
            project_id,
            input,
            focus_pending: true,
            _events: events,
        });
        cx.notify();
    }

    pub(super) fn submit_rename_project(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.rename_project_dialog.take() else {
            return;
        };
        let name = dialog.input.read(cx).text().trim().to_string();
        if !name.is_empty() {
            self.mutate(
                serde_json::json!({ "op": "renameProject", "projectId": dialog.project_id, "name": name }),
                cx,
            );
        }
        cx.notify();
    }

    pub(super) fn delete_project(&mut self, project_id: String, cx: &mut Context<Self>) {
        self.delete_project_confirm = None;
        self.mutate(
            serde_json::json!({ "op": "deleteProject", "projectId": project_id }),
            cx,
        );
        cx.notify();
    }

    /// Project context menu + rename dialog + delete confirm (appended to the
    /// shell's overlay list).
    pub(super) fn render_project_overlays(
        &mut self,
        viewport: gpui::Size<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let theme = Theme::of(cx).clone();
        let mut overlays: Vec<AnyElement> = Vec::new();

        if let Some((project_id, position)) = self.project_menu.clone() {
            let rename_id = project_id.clone();
            let delete_id = project_id.clone();
            let menu = popover::popover_card(&theme)
                .w(px(170.0))
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.project_menu = None;
                    cx.notify();
                }))
                .flex()
                .flex_col()
                .child(
                    popover::menu_row(&theme, false, format!("project-menu-rename-{project_id}"))
                        .id("project-menu-rename")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.open_rename_project(rename_id.clone(), cx)
                        }))
                        .child(icon(icons::PEN).size(px(16.0)).text_color(theme.text_muted))
                        .child(SharedString::from("Rename…")),
                )
                .child(popover::menu_separator())
                .child(
                    popover::menu_row(&theme, false, format!("project-menu-delete-{project_id}"))
                        .id("project-menu-delete")
                        .text_color(theme.danger)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.project_menu = None;
                            this.delete_project_confirm = Some(delete_id.clone());
                            cx.notify();
                        }))
                        .child(
                            icon(icons::TRASH_BIN_MINIMALISTIC)
                                .size(px(16.0))
                                .text_color(theme.danger),
                        )
                        .child(SharedString::from("Remove…")),
                )
                .into_any_element();
            overlays.push(popover::menu_at("project-context-menu", position, menu));
        }

        if let Some(dialog) = &mut self.rename_project_dialog {
            if std::mem::take(&mut dialog.focus_pending) {
                window.focus(&dialog.input.focus_handle(cx), cx);
            }
            let input = dialog.input.clone();
            let card = popover::dialog_card(&theme)
                .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, _, cx| {
                    if ev.keystroke.key == "escape" {
                        this.rename_project_dialog = None;
                        cx.notify();
                    }
                }))
                .child(popover::dialog_title(&theme, "Rename project"))
                .child(
                    div()
                        .mt(px(12.0))
                        .child(popover::dialog_field(input.into_any_element())),
                )
                .child(
                    div()
                        .mt(px(16.0))
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            popover::btn_ghost(&theme, "Cancel", "rename-project-cancel")
                                .id("rename-project-cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.rename_project_dialog = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            popover::btn_primary(&theme, "Rename")
                                .id("rename-project-save")
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.submit_rename_project(cx)),
                                ),
                        ),
                )
                .into_any_element();
            overlays.push(popover::modal("rename-project-dialog", viewport, card));
        }

        if let Some(project_id) = self.delete_project_confirm.clone() {
            let (name, count) = {
                let state = self.state.read(cx);
                let project = state.project_row(&project_id);
                (
                    project
                        .map(|s| s.display_name().to_string())
                        .unwrap_or_else(|| "this project".into()),
                    state.chats_in_project(&project_id).len(),
                )
            };
            let copy = if count == 1 {
                format!(
                    "Removing “{name}” permanently deletes its 1 session. This can’t be undone."
                )
            } else {
                format!(
                    "Removing “{name}” permanently deletes its {count} sessions. This can’t be undone."
                )
            };
            let card = popover::dialog_card(&theme)
                .child(popover::dialog_title(&theme, "Remove project?"))
                .child(div().mt(px(6.0)).child(popover::dialog_body(&theme, copy)))
                .child(
                    div()
                        .mt(px(16.0))
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            popover::btn_ghost(&theme, "Cancel", "delete-project-cancel")
                                .id("delete-project-cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.delete_project_confirm = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            popover::btn_danger(&theme, "Remove")
                                .id("delete-project-confirm")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.delete_project(project_id.clone(), cx)
                                })),
                        ),
                )
                .into_any_element();
            overlays.push(popover::modal("delete-project-dialog", viewport, card));
        }

        overlays
    }
}
