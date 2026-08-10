//! Settings → Developer: dev-loop knobs. Today that's the hot-reload toggle
//! for scripts/nova-dev.sh (warm window handoff — the replacement process
//! restores the open chat and signals readiness before the old one exits).

use gpui::{Context, EventEmitter, SharedString, Window, div, prelude::*, px};

use crate::settings::widgets;
use crate::theme::Theme;

/// Persisted + applied by the shell (same shape as [`AppearanceEvent`]).
#[derive(Debug, Clone)]
pub enum DeveloperEvent {
    HotReloadChanged(bool),
}

pub struct DeveloperPage {
    hot_reload: bool,
}

impl EventEmitter<DeveloperEvent> for DeveloperPage {}

impl DeveloperPage {
    pub fn new(hot_reload: bool, _cx: &mut Context<Self>) -> Self {
        Self { hot_reload }
    }

    /// Pill switch (36×22, knob slides right when on) — the settings pages
    /// have no other boolean rows yet, so the switch is local to this page.
    /// The track always carries a border so it reads as a control on both
    /// schemes; the knob contrasts against the track in both states.
    fn toggle(theme: &Theme, on: bool) -> gpui::Div {
        div()
            .w(px(36.0))
            .h(px(22.0))
            .rounded_full()
            .border_1()
            .border_color(if on {
                theme.accent
            } else {
                theme.border_strong
            })
            .bg(if on {
                theme.accent
            } else {
                crate::theme::wash(0.08)
            })
            .flex()
            .items_center()
            .when(on, |el| el.flex_row_reverse())
            .p(px(3.0))
            .child(
                div()
                    .w(px(14.0))
                    .h(px(14.0))
                    .rounded_full()
                    .bg(if on { gpui::white() } else { theme.text_muted })
                    .flex_none(),
            )
    }
}

impl Render for DeveloperPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let on = self.hot_reload;

        widgets::page_column()
            .child(widgets::page_header(&theme, "Developer", None))
            .child(widgets::page_subtitle(
                &theme,
                "Knobs for developing Nova itself. No effect on normal use.",
            ))
            .child(
                widgets::section_card(&theme).child(
                    widgets::card_row(&theme, true)
                        .id("developer-hot-reload")
                        .cursor_pointer()
                        .items_center()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.hot_reload = !this.hot_reload;
                            cx.emit(DeveloperEvent::HotReloadChanged(this.hot_reload));
                            cx.notify();
                        }))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(2.0))
                                .flex_1()
                                .child(widgets::row_title(&theme, "Hot reload"))
                                .child(
                                    div()
                                        .text_size(px(12.0))
                                        .text_color(theme.text_muted)
                                        .child(SharedString::from(
                                            "Reopened windows restore the current chat and \
                                             report readiness to scripts/nova-dev.sh before the \
                                             old window exits.",
                                        )),
                                ),
                        )
                        .child(Self::toggle(&theme, on)),
                ),
            )
    }
}
