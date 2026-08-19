//! Native Pi parent/child collaboration surface.

use gpui::{
    AnyElement, Context, Entity, IntoElement, Render, SharedString, Subscription, Task, Window,
    div, prelude::*, px,
};

use nova_doc::{MessagePart, MessageRole};
use nova_proto::{
    ChildAgent, ChildAgentStatus, CollaborationAction, CollaborationControlReply,
    CollaborationControlRequest, CollaborationSpeaker,
};
use nova_rpc::methods;

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::icons::{self, icon};
use crate::state::AppState;
use crate::theme::Theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ComposeMode {
    Room,
    Child,
    Resume,
    Spawn,
}

pub struct CollaborationPanel {
    state: Entity<AppState>,
    input: Entity<ComposerInput>,
    selected_child: Option<String>,
    mode: ComposeMode,
    agent: &'static str,
    busy: bool,
    notice: Option<(bool, SharedString)>,
    request_task: Option<Task<()>>,
    _input_events: Subscription,
    _state_observation: Subscription,
}

impl CollaborationPanel {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| ComposerInput::new("Message parent and child", cx));
        let input_events = cx.subscribe(&input, |this: &mut Self, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.submit(cx);
            }
        });
        let state_observation = cx.observe(&state, |this: &mut Self, state, cx| {
            let children = state
                .read(cx)
                .selected_collaboration()
                .map(|collaboration| collaboration.children.as_slice())
                .unwrap_or_default();
            if this
                .selected_child
                .as_deref()
                .is_some_and(|selected| !children.iter().any(|child| child.id == selected))
            {
                this.selected_child = None;
            }
            if this.selected_child.is_none() {
                this.selected_child = children
                    .iter()
                    .find(|child| {
                        matches!(
                            child.status,
                            ChildAgentStatus::Working | ChildAgentStatus::NeedsAttention
                        )
                    })
                    .or_else(|| children.last())
                    .map(|child| child.id.clone());
            }
            cx.notify();
        });
        Self {
            state,
            input,
            selected_child: None,
            mode: ComposeMode::Room,
            agent: "worker",
            busy: false,
            notice: None,
            request_task: None,
            _input_events: input_events,
            _state_observation: state_observation,
        }
    }

    fn select_mode(&mut self, mode: ComposeMode, cx: &mut Context<Self>) {
        self.mode = mode;
        let placeholder = match mode {
            ComposeMode::Room => "Message parent and child",
            ComposeMode::Child => "Steer this child",
            ComposeMode::Resume => "Give this child its next mission",
            ComposeMode::Spawn => "Describe the child mission",
        };
        self.input
            .update(cx, |input, cx| input.set_placeholder(placeholder, cx));
        self.notice = None;
        cx.notify();
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let message = self.input.read(cx).text().trim().to_owned();
        if message.is_empty() {
            return;
        }
        let (chat_id, device_id, engine) = {
            let state = self.state.read(cx);
            let Some(chat) = state.selected_chat_row() else {
                return;
            };
            let Some(engine) = state.engine().cloned() else {
                self.notice = Some((false, "Engine not connected".into()));
                cx.notify();
                return;
            };
            (chat.id.clone(), chat.device_id.clone(), engine)
        };
        let action = match self.mode {
            ComposeMode::Room => CollaborationAction::Room,
            ComposeMode::Child => CollaborationAction::Steer,
            ComposeMode::Resume => CollaborationAction::Resume,
            ComposeMode::Spawn => CollaborationAction::Spawn,
        };
        let request = CollaborationControlRequest {
            chat_id,
            action,
            child_id: self.selected_child.clone(),
            agent: (self.mode == ComposeMode::Spawn).then(|| self.agent.to_owned()),
            message: Some(message),
        };
        let mut params = serde_json::to_value(request).unwrap_or_default();
        if let Some(object) = params.as_object_mut() {
            object.insert("targetDeviceId".into(), device_id.into());
        }
        self.busy = true;
        self.notice = None;
        self.request_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::COLLABORATION_CONTROL, params)
                .await
                .and_then(|value| {
                    serde_json::from_value::<CollaborationControlReply>(value)
                        .map_err(|error| nova_rpc::RpcError::Transport(error.to_string()))
                });
            this.update(cx, |panel, cx| {
                panel.busy = false;
                match result {
                    Ok(reply) => {
                        panel.notice = Some((true, reply.message.into()));
                        panel
                            .input
                            .update(cx, |input, cx| input.set_text(String::new(), cx));
                        if panel.mode == ComposeMode::Spawn {
                            panel.select_mode(ComposeMode::Room, cx);
                        }
                    }
                    Err(error) => panel.notice = Some((false, error.to_string().into())),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn control_child(&mut self, action: CollaborationAction, cx: &mut Context<Self>) {
        let Some(child_id) = self.selected_child.clone() else {
            return;
        };
        let (chat_id, device_id, engine) = {
            let state = self.state.read(cx);
            let Some(chat) = state.selected_chat_row() else {
                return;
            };
            let Some(engine) = state.engine().cloned() else {
                return;
            };
            (chat.id.clone(), chat.device_id.clone(), engine)
        };
        let mut params = serde_json::to_value(CollaborationControlRequest {
            chat_id,
            action,
            child_id: Some(child_id),
            agent: None,
            message: None,
        })
        .unwrap_or_default();
        if let Some(object) = params.as_object_mut() {
            object.insert("targetDeviceId".into(), device_id.into());
        }
        self.busy = true;
        self.request_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::COLLABORATION_CONTROL, params)
                .await;
            this.update(cx, |panel, cx| {
                panel.busy = false;
                panel.notice = Some(match result {
                    Ok(_) => (true, "control delivered".into()),
                    Err(error) => (false, error.to_string().into()),
                });
                cx.notify();
            })
            .ok();
        }));
    }

    fn tiny_button(
        &self,
        id: impl Into<gpui::ElementId>,
        label: impl Into<SharedString>,
        selected: bool,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let theme = Theme::of(cx);
        let label: SharedString = label.into();
        div()
            .id(id)
            .h(px(26.0))
            .px(px(9.0))
            .flex()
            .items_center()
            .rounded(px(7.0))
            .bg(if selected {
                theme.element_active
            } else {
                theme.element_hover.opacity(0.45)
            })
            .hover(|style| style.bg(theme.element_hover))
            .text_size(px(11.0))
            .text_color(if selected {
                theme.text
            } else {
                theme.text_muted
            })
            .cursor_pointer()
            .child(label)
    }

    fn child_row(&self, child: &ChildAgent, selected: bool, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let status_color = match child.status {
            ChildAgentStatus::Working | ChildAgentStatus::Starting => theme.accent,
            ChildAgentStatus::NeedsAttention => theme.warning,
            ChildAgentStatus::Failed => theme.danger,
            ChildAgentStatus::Completed | ChildAgentStatus::Stopped => theme.text_faint,
        };
        let status = match child.status {
            ChildAgentStatus::Starting => "starting",
            ChildAgentStatus::Working => "working",
            ChildAgentStatus::NeedsAttention => "needs you",
            ChildAgentStatus::Completed => "complete",
            ChildAgentStatus::Failed => "failed",
            ChildAgentStatus::Stopped => "stopped",
        };
        let id = child.id.clone();
        let running = matches!(
            child.status,
            ChildAgentStatus::Starting | ChildAgentStatus::Working
        );
        div()
            .id(SharedString::from(format!("collaboration-child-{id}")))
            .w_full()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(9.0))
            .px(px(10.0))
            .py(px(9.0))
            .rounded(px(9.0))
            .border_1()
            .border_color(if selected {
                theme.accent.opacity(0.35)
            } else {
                theme.border.opacity(0.55)
            })
            .when(selected, |el| el.bg(theme.element_active))
            .hover(|style| style.bg(theme.element_hover))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                this.selected_child = Some(id.clone());
                if matches!(this.mode, ComposeMode::Spawn | ComposeMode::Resume) {
                    this.select_mode(ComposeMode::Room, cx);
                }
                cx.notify();
            }))
            .child(
                div()
                    .size(px(24.0))
                    .rounded(px(7.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(status_color.opacity(0.12))
                    .child(icon(icons::BOT).size(px(14.0)).text_color(status_color)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(
                        div()
                            .text_size(px(12.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(SharedString::from(child.agent.clone())),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(px(10.0))
                            .text_color(theme.text_faint)
                            .child(SharedString::from(
                                child
                                    .activity
                                    .clone()
                                    .or_else(|| child.goal.clone())
                                    .unwrap_or_else(|| status.into()),
                            )),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .child(
                        div()
                            .text_size(px(10.0))
                            .text_color(status_color)
                            .child(SharedString::from(status)),
                    )
                    .when(running, |el| {
                        el.child(crate::loaders::mini_gradient_spinner(
                            SharedString::from(format!("collaboration-child-working-{}", child.id)),
                            1.8,
                        ))
                    }),
            )
            .into_any_element()
    }
}

impl Render for CollaborationPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let collaboration = self.state.read(cx).selected_collaboration().cloned();
        let (available, parent_working, children, messages) = collaboration
            .map(|state| {
                (
                    state.available,
                    state.parent_working,
                    state.children,
                    state.messages,
                )
            })
            .unwrap_or_else(|| (false, false, Vec::new(), Vec::new()));
        if self.selected_child.is_none() {
            self.selected_child = children.first().map(|child| child.id.clone());
        }
        let selected = self
            .selected_child
            .as_deref()
            .and_then(|id| children.iter().find(|child| child.id == id));

        let parent_reply = self
            .state
            .read(cx)
            .transcript
            .iter()
            .rev()
            .find(|entry| entry.role == MessageRole::Assistant)
            .map(|entry| {
                entry
                    .parts
                    .iter()
                    .filter_map(|part| match part {
                        MessagePart::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .filter(|text| !text.trim().is_empty());

        let working_children = children
            .iter()
            .filter(|child| {
                matches!(
                    child.status,
                    ChildAgentStatus::Starting | ChildAgentStatus::Working
                )
            })
            .count();
        let topology = div()
            .flex_none()
            .px(px(12.0))
            .pt(px(12.0))
            .pb(px(10.0))
            .flex()
            .flex_col()
            .gap(px(5.0))
            .child(
                div()
                    .h(px(24.0))
                    .px(px(10.0))
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(10.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text_faint)
                            .child("AGENTS"),
                    )
                    .child(
                        div()
                            .text_size(px(10.0))
                            .text_color(if working_children > 0 {
                                theme.accent
                            } else {
                                theme.text_faint
                            })
                            .child(SharedString::from(format!("{} active", working_children))),
                    ),
            )
            .child(
                div()
                    .h(px(38.0))
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .px(px(10.0))
                    .child(icon(icons::BOT).size(px(16.0)).text_color(theme.text))
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(theme.text)
                                    .child("parent"),
                            )
                            .child(
                                div()
                                    .text_size(px(10.0))
                                    .text_color(theme.text_faint)
                                    .child(if parent_working {
                                        "coordinating"
                                    } else {
                                        "ready"
                                    }),
                            ),
                    )
                    .child(div().size(px(7.0)).rounded_full().bg(if parent_working {
                        theme.accent
                    } else {
                        theme.text_faint
                    })),
            )
            .child(
                div()
                    .ml(px(13.0))
                    .h(px(8.0))
                    .border_l_1()
                    .border_color(theme.border),
            )
            .children(children.iter().map(|child| {
                self.child_row(child, self.selected_child.as_deref() == Some(&child.id), cx)
            }))
            .child(
                self.tiny_button(
                    "collaboration-new-child",
                    "+ new child",
                    self.mode == ComposeMode::Spawn,
                    cx,
                )
                .mt(px(3.0))
                .on_click(cx.listener(|this, _, _, cx| this.select_mode(ComposeMode::Spawn, cx))),
            );

        let room_messages = messages
            .iter()
            .filter(|message| {
                message.child_id.is_none()
                    || message.child_id.as_deref() == self.selected_child.as_deref()
            })
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .take(20)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(|message| {
                let (label, color) = match message.speaker {
                    CollaborationSpeaker::You => ("you", theme.text),
                    CollaborationSpeaker::Parent => ("parent", theme.accent),
                    CollaborationSpeaker::Child => (
                        selected
                            .map(|child| child.agent.as_str())
                            .unwrap_or("child"),
                        theme.warning,
                    ),
                    CollaborationSpeaker::System => ("room", theme.text_faint),
                };
                div()
                    .flex()
                    .flex_col()
                    .gap(px(3.0))
                    .child(
                        div()
                            .text_size(px(10.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(color)
                            .child(SharedString::from(label.to_owned())),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .line_height(px(17.0))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(message.body.clone())),
                    )
                    .into_any_element()
            })
            .collect::<Vec<_>>();

        let modes = div()
            .flex()
            .gap(px(4.0))
            .child(
                self.tiny_button(
                    "collaboration-mode-room",
                    "room",
                    self.mode == ComposeMode::Room,
                    cx,
                )
                .on_click(cx.listener(|this, _, _, cx| this.select_mode(ComposeMode::Room, cx))),
            )
            .child(
                self.tiny_button(
                    "collaboration-mode-child",
                    "child only",
                    self.mode == ComposeMode::Child,
                    cx,
                )
                .on_click(cx.listener(|this, _, _, cx| this.select_mode(ComposeMode::Child, cx))),
            );

        let composer = div()
            .flex_none()
            .p(px(10.0))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .border_t_1()
            .border_color(theme.border)
            .when(self.mode == ComposeMode::Spawn, |el| {
                el.child(
                    div().flex().gap(px(4.0)).children(
                        ["worker", "scout", "researcher", "reviewer", "oracle"]
                            .into_iter()
                            .map(|agent| {
                                self.tiny_button(
                                    SharedString::from(format!("collaboration-agent-{agent}")),
                                    agent,
                                    self.agent == agent,
                                    cx,
                                )
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.agent = agent;
                                        cx.notify();
                                    },
                                ))
                            }),
                    ),
                )
            })
            .when(self.mode != ComposeMode::Spawn, |el| el.child(modes))
            .child(
                div()
                    .flex()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(10.0))
                            .text_color(
                                self.notice
                                    .as_ref()
                                    .map(
                                        |(ok, _)| if *ok { theme.text_faint } else { theme.danger },
                                    )
                                    .unwrap_or(theme.text_faint),
                            )
                            .child(
                                self.notice
                                    .as_ref()
                                    .map(|(_, text)| text.clone())
                                    .unwrap_or_else(|| {
                                        if available {
                                            "return to send".into()
                                        } else {
                                            "start a pi session to connect".into()
                                        }
                                    }),
                            ),
                    )
                    .child(
                        self.tiny_button(
                            "collaboration-send",
                            if self.busy { "sending" } else { "send" },
                            true,
                            cx,
                        )
                        .when(self.busy, |el| el.cursor_default().opacity(0.5))
                        .when(!self.busy, |el| {
                            el.on_click(cx.listener(|this, _, _, cx| this.submit(cx)))
                        }),
                    ),
            )
            .child(
                div()
                    .min_h(px(38.0))
                    .px(px(11.0))
                    .py(px(9.0))
                    .rounded(px(9.0))
                    .bg(theme.element_hover.opacity(0.5))
                    .child(self.input.clone()),
            );

        div()
            .size_full()
            .flex()
            .flex_col()
            .child(topology)
            .child(
                div()
                    .id("collaboration-room-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(px(16.0))
                    .py(px(14.0))
                    .flex()
                    .flex_col()
                    .gap(px(14.0))
                    .when(room_messages.is_empty() && parent_reply.is_none(), |el| {
                        el.child(
                            div()
                                .mt(px(24.0))
                                .text_size(px(12.0))
                                .text_color(theme.text_faint)
                                .child("Select a child, then talk to everyone in one room."),
                        )
                    })
                    .children(room_messages)
                    .when_some(parent_reply, |el, reply| {
                        el.child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(3.0))
                                .child(
                                    div()
                                        .text_size(px(10.0))
                                        .font_weight(gpui::FontWeight::MEDIUM)
                                        .text_color(theme.accent)
                                        .child("parent · latest"),
                                )
                                .child(
                                    div()
                                        .text_size(px(12.0))
                                        .line_height(px(17.0))
                                        .text_color(theme.text_muted)
                                        .child(SharedString::from(reply)),
                                ),
                        )
                    })
                    .when_some(selected.cloned(), |el, child| {
                        let live = matches!(
                            child.status,
                            ChildAgentStatus::Starting
                                | ChildAgentStatus::Working
                                | ChildAgentStatus::NeedsAttention
                        );
                        el.child(
                            div()
                                .mt(px(6.0))
                                .flex()
                                .gap(px(5.0))
                                .when(live, |el| {
                                    el.child(
                                        self.tiny_button("collaboration-stop", "stop", false, cx)
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.control_child(CollaborationAction::Stop, cx)
                                            })),
                                    )
                                })
                                .when(!live, |el| {
                                    el.child(
                                        self.tiny_button(
                                            "collaboration-resume",
                                            "resume with message",
                                            false,
                                            cx,
                                        )
                                        .on_click(
                                            cx.listener(|this, _, _, cx| {
                                                this.select_mode(ComposeMode::Resume, cx)
                                            }),
                                        ),
                                    )
                                }),
                        )
                    }),
            )
            .child(composer)
    }
}
