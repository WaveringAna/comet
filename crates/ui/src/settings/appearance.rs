//! Settings → Appearance: theme scheme/colors and the typefaces used by the
//! interface and code.
//!
//! The theme section drives [`ThemeConfig`]: a System/Light/Dark preference,
//! custom background + foreground hexes, and a contrast percentage. Changes
//! are applied to the live theme immediately and persisted by the shell.
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
use crate::settings::{DEFAULT_CODE_FONT, DEFAULT_UI_FONT};
use crate::theme::{
    ColorScheme, Theme, ThemeConfig, ThemePreference, default_bg_hex, default_fg_hex, parse_hex,
};

/// Always available in this app because Geist is bundled with the UI.
const BUNDLED_FONTS: [&str; 2] = ["Geist", "Geist Mono"];

/// Every Appearance row lands its control in the same right-hand column.
const CONTROL_COLUMN_WIDTH: f32 = 300.0;
const CONTRAST_VALUE_WIDTH: f32 = 44.0;
const CONTRAST_CONTROL_GAP: f32 = 12.0;

/// Drag marker for the contrast slider (gpui drag-and-drop idiom, like the
/// shell's resize handles). The drag ghost is an empty view; the root's
/// `on_drag_move::<ContrastDrag>` reads the slider's bounds from the event.
struct ContrastDrag;

/// Invisible drag ghost — the contrast drag renders nothing at the cursor.
struct AppearanceDragGhost;

impl Render for AppearanceDragGhost {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

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
    Changed {
        ui_font: String,
        code_font: String,
        preference: ThemePreference,
        bg_hex: Option<String>,
        fg_hex: Option<String>,
        contrast: f32,
    },
}

pub struct AppearancePage {
    preference: ThemePreference,
    /// Committed custom hexes; `None` means "follow the active scheme's default".
    bg_hex: Option<String>,
    fg_hex: Option<String>,
    contrast: f32,
    /// The scheme the page resolves scheme defaults against (the scheme the
    /// installed theme was built with — for `System`, the OS appearance).
    active_scheme: ColorScheme,
    ui_input: Entity<ComposerInput>,
    code_input: Entity<ComposerInput>,
    bg_input: Entity<ComposerInput>,
    fg_input: Entity<ComposerInput>,
    /// Names reported by the OS text system, plus the bundled families.
    available_fonts: Vec<String>,
    open_menu: Option<FontKind>,
    /// Prevent the trigger click that follows an outside mouse-down from
    /// immediately reopening the menu.
    menu_dismissed_at: Option<Instant>,
    _ui_events: Subscription,
    _code_events: Subscription,
    _bg_events: Subscription,
    _fg_events: Subscription,
}

impl EventEmitter<AppearanceEvent> for AppearancePage {}

impl AppearancePage {
    pub fn new(config: &ThemeConfig, cx: &mut Context<Self>) -> Self {
        let active_scheme = Theme::of(cx).scheme;
        let mut available_fonts = cx.text_system().all_font_names();
        available_fonts.extend(BUNDLED_FONTS.iter().map(|font| (*font).to_string()));
        available_fonts.push(config.ui_font.clone());
        available_fonts.push(config.code_font.clone());
        available_fonts.sort_unstable_by_key(|font| font.to_lowercase());
        available_fonts.dedup();

        let ui_input = cx.new(|cx| ComposerInput::new("Type a UI font family", cx));
        ui_input.update(cx, |input, cx| input.set_text(config.ui_font.clone(), cx));
        let code_input = cx.new(|cx| ComposerInput::new("Type a code font family", cx));
        code_input.update(cx, |input, cx| input.set_text(config.code_font.clone(), cx));
        let bg_input = cx.new(|cx| ComposerInput::new("Type a hex like #0a0a0a", cx));
        bg_input.update(cx, |input, cx| {
            input.set_text(config.effective_bg_hex(active_scheme), cx)
        });
        let fg_input = cx.new(|cx| ComposerInput::new("Type a hex like #e5e5e5", cx));
        fg_input.update(cx, |input, cx| {
            input.set_text(config.effective_fg_hex(active_scheme), cx)
        });

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
        let bg_events = cx.subscribe(&bg_input, |this, _, event, cx| match event {
            ComposerInputEvent::Edited | ComposerInputEvent::Submitted => {
                this.sync_hex_edits(cx);
                this.commit(cx);
            }
            _ => {}
        });
        let fg_events = cx.subscribe(&fg_input, |this, _, event, cx| match event {
            ComposerInputEvent::Edited | ComposerInputEvent::Submitted => {
                this.sync_hex_edits(cx);
                this.commit(cx);
            }
            _ => {}
        });

        Self {
            preference: config.preference,
            bg_hex: config.bg_hex.clone(),
            fg_hex: config.fg_hex.clone(),
            contrast: config.contrast,
            active_scheme,
            ui_input,
            code_input,
            bg_input,
            fg_input,
            available_fonts,
            open_menu: None,
            menu_dismissed_at: None,
            _ui_events: ui_events,
            _code_events: code_events,
            _bg_events: bg_events,
            _fg_events: fg_events,
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

    /// The scheme the preference resolves to right now (defaults follow it).
    fn resolved_scheme(&self) -> ColorScheme {
        self.preference.resolved(self.active_scheme)
    }

    /// Fold valid hex edits into the committed custom values. A value equal to
    /// the scheme's default is treated as "untouched" so switching schemes
    /// keeps following the new default until the user types something of their
    /// own.
    fn sync_hex_edits(&mut self, cx: &mut Context<Self>) {
        let bg_text = self.bg_input.read(cx).text().to_string();
        if parse_hex(&bg_text).is_some()
            && self.bg_hex.as_deref() != Some(bg_text.as_str())
            && !bg_text.eq_ignore_ascii_case(default_bg_hex(self.resolved_scheme()))
        {
            self.bg_hex = Some(bg_text);
        }
        let fg_text = self.fg_input.read(cx).text().to_string();
        if parse_hex(&fg_text).is_some()
            && self.fg_hex.as_deref() != Some(fg_text.as_str())
            && !fg_text.eq_ignore_ascii_case(default_fg_hex(self.resolved_scheme()))
        {
            self.fg_hex = Some(fg_text);
        }
    }

    fn commit(&mut self, cx: &mut Context<Self>) {
        cx.emit(AppearanceEvent::Changed {
            ui_font: self.value(FontKind::Ui, cx),
            code_font: self.value(FontKind::Code, cx),
            preference: self.preference,
            bg_hex: self.bg_hex.clone(),
            fg_hex: self.fg_hex.clone(),
            contrast: self.contrast,
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

    fn set_preference(&mut self, preference: ThemePreference, cx: &mut Context<Self>) {
        self.preference = preference;
        // Untouched hex fields follow the newly resolved scheme's default.
        if self.bg_hex.is_none() {
            self.bg_input.update(cx, |input, cx| {
                input.set_text(default_bg_hex(self.resolved_scheme()), cx)
            });
        }
        if self.fg_hex.is_none() {
            self.fg_input.update(cx, |input, cx| {
                input.set_text(default_fg_hex(self.resolved_scheme()), cx)
            });
        }
        self.commit(cx);
    }

    fn set_contrast(&mut self, contrast: f32, cx: &mut Context<Self>) {
        self.contrast = contrast.round().clamp(0.0, 100.0);
        self.commit(cx);
    }

    fn dismiss_menu(&mut self, cx: &mut Context<Self>) {
        self.open_menu = None;
        self.menu_dismissed_at = Some(Instant::now());
        cx.notify();
    }

    fn restore_defaults(&mut self, cx: &mut Context<Self>) {
        self.ui_input
            .update(cx, |input, cx| input.set_text(DEFAULT_UI_FONT, cx));
        self.code_input
            .update(cx, |input, cx| input.set_text(DEFAULT_CODE_FONT, cx));
        self.preference = ThemePreference::System;
        self.bg_hex = None;
        self.fg_hex = None;
        self.contrast = 100.0;
        let scheme = self.resolved_scheme();
        self.bg_input
            .update(cx, |input, cx| input.set_text(default_bg_hex(scheme), cx));
        self.fg_input
            .update(cx, |input, cx| input.set_text(default_fg_hex(scheme), cx));
        self.open_menu = None;
        self.menu_dismissed_at = None;
        self.commit(cx);
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
            .w(px(CONTROL_COLUMN_WIDTH))
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
            .bg(theme.wash(if menu_open { 0.07 } else { 0.03 }))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .hover(|s| s.border_color(theme.border_strong).bg(theme.wash(0.06)))
            // ComposerInput renders at full width. Constrain it inside a flex
            // child so the trailing chevron stays inside the control instead
            // of being pushed past its right edge.
            .child(div().flex_1().min_w_0().child(input))
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
            .child(self.label_column(title, description, theme))
            .child(trigger)
            .into_any_element()
    }

    /// The left-hand title + description column used by every row.
    fn label_column(
        &self,
        title: &'static str,
        description: &'static str,
        theme: &Theme,
    ) -> AnyElement {
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
            )
            .into_any_element()
    }

    /// The System/Light/Dark segmented control.
    fn render_scheme_row(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let description = match self.preference {
            ThemePreference::System => {
                "Follows your system appearance — switching the OS theme re-paints Comet live."
            }
            ThemePreference::Light => "Always use the light scheme, regardless of the system.",
            ThemePreference::Dark => "Always use the dark scheme, regardless of the system.",
        };
        let options = [
            ThemePreference::System,
            ThemePreference::Light,
            ThemePreference::Dark,
        ];
        div()
            .px(px(20.0))
            .py(px(16.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(16.0))
            .child(self.label_column("Scheme", description, theme))
            .child(
                div()
                    .id("appearance-scheme-segmented")
                    .w(px(CONTROL_COLUMN_WIDTH))
                    .flex_none()
                    .flex()
                    .flex_row()
                    .p(px(3.0))
                    .gap(px(2.0))
                    .rounded(px(9.0))
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.wash(0.03))
                    .children(options.into_iter().map(|option| {
                        let selected = self.preference == option;
                        div()
                            .id(SharedString::from(format!(
                                "appearance-scheme-{}",
                                option.label().to_lowercase()
                            )))
                            .flex_1()
                            .flex()
                            .justify_center()
                            .px(px(10.0))
                            .py(px(5.0))
                            .rounded(px(6.0))
                            .text_size(px(12.5))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(if selected {
                                theme.text
                            } else {
                                theme.text_muted
                            })
                            .bg(if selected {
                                theme.wash(0.12)
                            } else {
                                gpui::transparent_black()
                            })
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.set_preference(option, cx);
                            }))
                            .child(SharedString::from(option.label()))
                    })),
            )
            .into_any_element()
    }

    /// One hex row: a monospace input in a trigger-style box plus a live
    /// swatch of the color it currently spells.
    fn render_hex_row(
        &mut self,
        is_bg: bool,
        title: &'static str,
        description: &'static str,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (input, raw_text) = if is_bg {
            (
                self.bg_input.clone(),
                self.bg_input.read(cx).text().to_string(),
            )
        } else {
            (
                self.fg_input.clone(),
                self.fg_input.read(cx).text().to_string(),
            )
        };
        let swatch = parse_hex(&raw_text).unwrap_or(theme.surface_raised);
        let trigger = div()
            .id(SharedString::from(format!(
                "appearance-hex-trigger-{}",
                if is_bg { "bg" } else { "fg" }
            )))
            .relative()
            .w(px(CONTROL_COLUMN_WIDTH))
            .flex_none()
            .px(px(10.0))
            .py(px(8.0))
            .rounded(px(8.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.wash(0.03))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .hover(|s| s.border_color(theme.border_strong).bg(theme.wash(0.06)))
            .child(div().flex_1().min_w_0().child(input))
            .child(
                div()
                    .flex_none()
                    .size(px(16.0))
                    .rounded(px(5.0))
                    .border_1()
                    .border_color(theme.border_strong)
                    .bg(swatch),
            );

        div()
            .px(px(20.0))
            .py(px(16.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(16.0))
            .border_t_1()
            .border_color(theme.border)
            .child(self.label_column(title, description, theme))
            .child(trigger)
            .into_any_element()
    }

    /// The contrast slider: a draggable track whose fill/thumb track the
    /// percentage. Its drag listener lives on the slider itself, so the event
    /// bounds and cursor position share the same coordinate system.
    fn render_contrast_row(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let t = (self.contrast / 100.0).clamp(0.0, 1.0);
        let thumb_w = 14.0_f32;
        let track_w = CONTROL_COLUMN_WIDTH - CONTRAST_VALUE_WIDTH - CONTRAST_CONTROL_GAP;
        let rail_inset = thumb_w / 2.0;
        let rail_w = track_w - thumb_w;
        let fill_w = t * rail_w;
        let thumb_x = t * rail_w;
        let slider = div()
            .id("appearance-contrast-slider")
            .relative()
            .w(px(track_w))
            .h(px(28.0))
            .flex_none()
            .cursor_pointer()
            .on_drag(ContrastDrag, |_, _point, _, cx| {
                cx.stop_propagation();
                cx.new(|_| AppearanceDragGhost)
            })
            .on_drag_move::<ContrastDrag>(cx.listener(
                move |this: &mut Self, event: &gpui::DragMoveEvent<ContrastDrag>, _, cx| {
                    // Map the cursor to the thumb CENTER's travel, not the
                    // outer hitbox, so the thumb stays 1:1 under it.
                    let left = f32::from(event.bounds.left()) + rail_inset;
                    let travel = (f32::from(event.bounds.size.width) - thumb_w).max(1.0);
                    let t = ((f32::from(event.event.position.x) - left) / travel).clamp(0.0, 1.0);
                    this.set_contrast(t * 100.0, cx);
                },
            ))
            .child(
                div()
                    .absolute()
                    .top(px(13.0))
                    .left(px(rail_inset))
                    .w(px(rail_w))
                    .h(px(2.0))
                    .rounded_full()
                    .bg(theme.wash(0.10)),
            )
            .child(
                div()
                    .absolute()
                    .top(px(13.0))
                    .left(px(rail_inset))
                    .w(px(fill_w))
                    .h(px(2.0))
                    .rounded_full()
                    .bg(theme.text),
            )
            .child(
                div()
                    .absolute()
                    .top(px(6.0))
                    .left(px(thumb_x))
                    .size(px(thumb_w))
                    .rounded_full()
                    .border_1()
                    .border_color(theme.border_strong)
                    .bg(theme.input_bg())
                    .shadow_sm(),
            );
        let value = div()
            .flex_none()
            .w(px(CONTRAST_VALUE_WIDTH))
            .text_align(gpui::TextAlign::Right)
            .text_size(px(12.5))
            .text_color(theme.text)
            .child(SharedString::from(format!(
                "{}%",
                self.contrast.round() as u32
            )));

        div()
            .px(px(20.0))
            .py(px(16.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(16.0))
            .border_t_1()
            .border_color(theme.border)
            .child(self.label_column(
                "Contrast",
                "The tonal spread of secondary text, surfaces, and hairlines between the two hexes.",
                theme,
            ))
            .child(
                div()
                    .w(px(CONTROL_COLUMN_WIDTH))
                    .flex_none()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(CONTRAST_CONTROL_GAP))
                    .child(slider)
                    .child(value),
            )
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
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_start()
                        .justify_between()
                        .gap(px(24.0))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .child(crate::settings::widgets::page_header(
                                    &theme,
                                    "Appearance",
                                    None,
                                ))
                                .child(crate::settings::widgets::page_subtitle(
                                    &theme,
                                    "Theme colors and the typefaces Comet uses for interface text and code.",
                                )),
                        )
                        .child(
                            crate::settings::widgets::ghost_action(&theme)
                                .id("appearance-restore-defaults")
                                .flex_none()
                                .hover(|s| {
                                    s.bg(theme.wash(0.04)).text_color(theme.text)
                                })
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.restore_defaults(cx);
                                }))
                                .child(
                                    crate::icons::icon(crate::icons::RESTART)
                                        .size(px(14.0))
                                        .text_color(theme.text_muted),
                                )
                                .child(SharedString::from("Restore defaults")),
                        ),
                )
                .child(
                    crate::settings::widgets::section_card(&theme)
                        .child(self.render_scheme_row(&theme, cx))
                        .child(self.render_hex_row(
                            true,
                            "Background",
                            "The app background — main panel and window.",
                            &theme,
                            cx,
                        ))
                        .child(self.render_hex_row(
                            false,
                            "Foreground",
                            "Primary text. Secondary roles derive from it toward the background.",
                            &theme,
                            cx,
                        ))
                        .child(self.render_contrast_row(&theme, cx)),
                )
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
