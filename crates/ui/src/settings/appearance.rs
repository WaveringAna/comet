//! Settings → Appearance: choose the typefaces used by the interface and code.
//!
//! Font names come from gpui's platform text system, so the fields can accept
//! any family installed on the current OS. The suggestion menu is filtered by
//! the role: proportional families for UI text and families that advertise a
//! monospaced/code face for code.

use std::time::Instant;

use gpui::{
    AnyElement, Context, Entity, EventEmitter, SharedString, Subscription, Window, div, prelude::*,
    px,
};

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::popover;
use crate::theme::Theme;

/// Always available in this app because Geist is bundled with the UI.
const BUNDLED_FONTS: [&str; 2] = ["Geist", "Geist Mono"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FontKind {
    Ui,
    Code,
}

impl FontKind {
    fn label(self) -> &'static str {
        match self {
            Self::Ui => "ui",
            Self::Code => "code",
        }
    }

    fn heading(self) -> &'static str {
        match self {
            Self::Ui => "UI fonts",
            Self::Code => "Code fonts",
        }
    }
}

/// Changes are applied to the live theme and persisted by the shell.
#[derive(Debug, Clone)]
pub enum AppearanceEvent {
    Changed { ui_font: String, code_font: String },
}

pub struct AppearancePage {
    ui_input: Entity<ComposerInput>,
    code_input: Entity<ComposerInput>,
    /// Names reported by the OS text system, plus the bundled families.
    available_fonts: Vec<String>,
    open_menu: Option<FontKind>,
    /// Prevent the trigger click that follows an outside mouse-down from
    /// immediately reopening the menu.
    menu_dismissed_at: Option<Instant>,
    _ui_events: Subscription,
    _code_events: Subscription,
}

impl EventEmitter<AppearanceEvent> for AppearancePage {}

impl AppearancePage {
    pub fn new(ui_font: String, code_font: String, cx: &mut Context<Self>) -> Self {
        let mut available_fonts = cx.text_system().all_font_names();
        available_fonts.extend(BUNDLED_FONTS.iter().map(|font| (*font).to_string()));
        available_fonts.push(ui_font.clone());
        available_fonts.push(code_font.clone());
        available_fonts.sort_unstable_by_key(|font| font.to_lowercase());
        available_fonts.dedup();

        let ui_input = cx.new(|cx| ComposerInput::new("Type a UI font family", cx));
        ui_input.update(cx, |input, cx| input.set_text(ui_font, cx));
        let code_input = cx.new(|cx| ComposerInput::new("Type a code font family", cx));
        code_input.update(cx, |input, cx| input.set_text(code_font, cx));

        let ui_events = cx.subscribe(
            &ui_input,
            |this: &mut Self, _, event: &ComposerInputEvent, cx| match event {
                ComposerInputEvent::Edited => {
                    this.open_menu = Some(FontKind::Ui);
                    this.commit(cx);
                }
                ComposerInputEvent::Submitted => {
                    this.open_menu = None;
                    this.commit(cx);
                }
                _ => {}
            },
        );
        let code_events = cx.subscribe(
            &code_input,
            |this: &mut Self, _, event: &ComposerInputEvent, cx| match event {
                ComposerInputEvent::Edited => {
                    this.open_menu = Some(FontKind::Code);
                    this.commit(cx);
                }
                ComposerInputEvent::Submitted => {
                    this.open_menu = None;
                    this.commit(cx);
                }
                _ => {}
            },
        );

        Self {
            ui_input,
            code_input,
            available_fonts,
            open_menu: None,
            menu_dismissed_at: None,
            _ui_events: ui_events,
            _code_events: code_events,
        }
    }

    fn input(&self, kind: FontKind) -> &Entity<ComposerInput> {
        match kind {
            FontKind::Ui => &self.ui_input,
            FontKind::Code => &self.code_input,
        }
    }

    fn value(&self, kind: FontKind, cx: &Context<Self>) -> String {
        self.input(kind).read(cx).text().to_string()
    }

    fn commit(&self, cx: &mut Context<Self>) {
        cx.emit(AppearanceEvent::Changed {
            ui_font: self.value(FontKind::Ui, cx),
            code_font: self.value(FontKind::Code, cx),
        });
        cx.notify();
    }

    fn set_font(&mut self, kind: FontKind, font: String, cx: &mut Context<Self>) {
        self.input(kind)
            .update(cx, |input, cx| input.set_text(font, cx));
        self.open_menu = None;
        self.menu_dismissed_at = None;
        // set_text emits Edited and commits through the input subscription.
        cx.notify();
    }

    fn dismiss_menu(&mut self, cx: &mut Context<Self>) {
        self.open_menu = None;
        self.menu_dismissed_at = Some(Instant::now());
        cx.notify();
    }

    /// Best-effort role classification using the family name exposed by the
    /// OS. Platform text systems do not expose a cross-platform monospace trait
    /// through gpui, so code-oriented family names are the portable signal.
    fn is_monospace(name: &str) -> bool {
        let name = name.to_lowercase();
        [
            "mono",
            "monospace",
            "code",
            "courier",
            "menlo",
            "monaco",
            "consolas",
            "cascadia",
            "fixed",
            "terminal",
            "typewriter",
        ]
        .iter()
        .any(|marker| name.contains(marker))
    }

    fn matching_fonts(&self, kind: FontKind, query: &str) -> Vec<String> {
        let query = query.trim().to_lowercase();
        self.available_fonts
            .iter()
            .filter(|font| {
                let mono = Self::is_monospace(font);
                let role_matches = match kind {
                    FontKind::Ui => !mono,
                    FontKind::Code => mono,
                };
                role_matches && (query.is_empty() || font.to_lowercase().contains(&query))
            })
            .take(80)
            .cloned()
            .collect()
    }

    fn render_font_menu(
        &mut self,
        kind: FontKind,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let current = self.value(kind, cx);
        let matches = self.matching_fonts(kind, &current);
        let mut menu = popover::popover_card(theme)
            .w(px(300.0))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.dismiss_menu(cx)))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(popover::menu_heading(theme, kind.heading()));

        if matches.is_empty() {
            menu = menu.child(
                div()
                    .px(px(8.0))
                    .py(px(10.0))
                    .text_size(px(12.0))
                    .text_color(theme.text_muted)
                    .child(SharedString::from("No matching installed fonts")),
            );
        } else {
            menu = menu.children(matches.into_iter().enumerate().map(|(ix, option)| {
                let selected = current == option;
                let click_name = option.clone();
                popover::menu_row(
                    theme,
                    selected,
                    format!("appearance-font-{}-{ix}", kind.label()),
                )
                .id(SharedString::from(format!(
                    "appearance-font-option-{}-{ix}",
                    kind.label()
                )))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.set_font(kind, click_name.clone(), cx);
                }))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_family(option.clone())
                        .child(SharedString::from(option)),
                )
                .when(selected, |el| el.child(popover::menu_check(theme)))
            }));
        }

        popover::anchored_menu(
            format!("appearance-font-menu-{}", kind.label()),
            menu.into_any_element(),
        )
    }

    fn render_font_row(
        &mut self,
        kind: FontKind,
        title: &'static str,
        description: &'static str,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let menu_open = self.open_menu == Some(kind);
        let just_dismissed = self
            .menu_dismissed_at
            .is_some_and(|at| at.elapsed().as_millis() < 400);
        let input = self.input(kind).clone();
        let trigger_id = SharedString::from(format!("appearance-font-trigger-{}", kind.label()));
        let mut trigger = div()
            .id(trigger_id)
            .relative()
            .w(px(300.0))
            .flex_none()
            .px(px(10.0))
            .py(px(8.0))
            .rounded(px(8.0))
            .border_1()
            .border_color(if menu_open {
                theme.border_strong
            } else {
                theme.border
            })
            .bg(crate::theme::white_alpha(if menu_open {
                0.07
            } else {
                0.03
            }))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .hover(|s| {
                s.border_color(theme.border_strong)
                    .bg(crate::theme::white_alpha(0.06))
            })
            .child(input.into_any_element())
            .child(
                div()
                    .id(SharedString::from(format!(
                        "appearance-font-toggle-{}",
                        kind.label()
                    )))
                    .flex_none()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if just_dismissed {
                            this.menu_dismissed_at = None;
                        } else {
                            this.open_menu = if this.open_menu == Some(kind) {
                                None
                            } else {
                                Some(kind)
                            };
                        }
                        cx.notify();
                    }))
                    .child(
                        crate::icons::icon(crate::icons::ALT_ARROW_DOWN)
                            .size(px(14.0))
                            .text_color(theme.text_muted.opacity(0.7)),
                    ),
            );
        if menu_open {
            trigger = trigger.child(self.render_font_menu(kind, theme, cx));
        }

        div()
            .px(px(20.0))
            .py(px(16.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(16.0))
            .when(kind == FontKind::Code, |el| {
                el.border_t_1().border_color(theme.border)
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(3.0))
                    .child(
                        div()
                            .text_size(px(13.5))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(SharedString::from(title)),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .line_height(px(18.0))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(description)),
                    ),
            )
            .child(trigger)
            .into_any_element()
    }
}

impl Render for AppearancePage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        div()
            .id("appearance-page")
            .size_full()
            .overflow_y_scroll()
            .child(
                crate::settings::widgets::page_column()
                    .child(crate::settings::widgets::page_header(
                        &theme,
                        "Appearance",
                        None,
                    ))
                    .child(crate::settings::widgets::page_subtitle(
                        &theme,
                        "Choose the typefaces Comet uses for interface text and code.",
                    ))
                    .child(
                        crate::settings::widgets::section_card(&theme)
                            .child(self.render_font_row(
                                FontKind::Ui,
                                "UI font",
                                "Navigation, controls, messages, and other interface text.",
                                &theme,
                                cx,
                            ))
                            .child(self.render_font_row(
                                FontKind::Code,
                                "Code font",
                                "Code blocks, terminal output, and keyboard shortcuts.",
                                &theme,
                                cx,
                            )),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn family_role_detection_keeps_code_fonts_monospace() {
        assert!(AppearancePage::is_monospace("JetBrains Mono"));
        assert!(AppearancePage::is_monospace("DejaVu Sans Mono"));
        assert!(!AppearancePage::is_monospace("Noto Sans"));
    }
}
