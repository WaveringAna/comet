//! Monochrome theme engine — light/dark schemes derived from a background +
//! foreground hex pair and a contrast percentage.
//!
//! The default dark theme's tones are precomputed from an oklch-derived neutral
//! scale (perceptually even lightness steps; the same scale comet's Tailwind
//! theme used) into gpui [`Hsla`]. User themes are built by [`Theme::custom`]:
//! the background/foreground hexes anchor the ramp and every intermediate role
//! (panel surfaces, muted/faint text, hairlines, washes) is a fixed fraction of
//! the distance between them, scaled by the contrast percentage. Hairlines are
//! the scheme's text pole at low alpha so they read on any surface. **Numbers
//! drive layout, colors are paint**: layout constants live here as plain numbers
//! and never depend on which color is painted.
//!
//! Installed as a gpui [`Global`] at boot via [`Theme::install`]; read with
//! [`Theme::of`]. The scheme mirror that flips the [`wash`]/[`white_alpha`]
//! paint primitives is kept in a thread-local so element builders without a
//! `&Theme` still paint the active scheme.

use std::cell::Cell;

use gpui::{App, Global, Hsla, Rgba, SharedString, hsla};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Scheme / preference
// ---------------------------------------------------------------------------

/// The persisted theme preference (Settings → Appearance). `System` follows
/// the OS appearance live — switching the system theme re-paints the app
/// without a restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemePreference {
    #[default]
    System,
    Light,
    Dark,
}

impl ThemePreference {
    pub fn label(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::Light => "Light",
            Self::Dark => "Dark",
        }
    }

    /// Resolve the preference against the OS appearance into a concrete scheme.
    pub fn resolved(self, system: ColorScheme) -> ColorScheme {
        match self {
            Self::System => system,
            Self::Light => ColorScheme::Light,
            Self::Dark => ColorScheme::Dark,
        }
    }
}

/// A concrete paint scheme — the direction of the neutral ramp (light surfaces
/// with dark text vs dark surfaces with light text).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorScheme {
    Light,
    #[default]
    Dark,
}

impl ColorScheme {
    pub fn is_dark(self) -> bool {
        self == Self::Dark
    }
}

impl From<gpui::WindowAppearance> for ColorScheme {
    fn from(appearance: gpui::WindowAppearance) -> Self {
        match appearance {
            gpui::WindowAppearance::Dark | gpui::WindowAppearance::VibrantDark => Self::Dark,
            gpui::WindowAppearance::Light | gpui::WindowAppearance::VibrantLight => Self::Light,
        }
    }
}

// ---------------------------------------------------------------------------
// Defaults + derivation constants
// ---------------------------------------------------------------------------

/// Default dark background — sampled #060606 from the reference screenshots
/// (main panel).
pub const DEFAULT_BG_DARK: &str = "#060606";
/// Default dark foreground — the pre-2.0 text tone `neutral(0.922)` → #e5e5e5.
pub const DEFAULT_FG_DARK: &str = "#e5e5e5";
/// Default light background — a near-white panel.
pub const DEFAULT_BG_LIGHT: &str = "#fafafa";
/// Default light foreground — a near-black text tone.
pub const DEFAULT_FG_LIGHT: &str = "#161616";
/// Default contrast percentage (full tonal spread).
pub const DEFAULT_CONTRAST: f32 = 100.0;

pub fn default_bg_hex(scheme: ColorScheme) -> &'static str {
    match scheme {
        ColorScheme::Light => DEFAULT_BG_LIGHT,
        ColorScheme::Dark => DEFAULT_BG_DARK,
    }
}

pub fn default_fg_hex(scheme: ColorScheme) -> &'static str {
    match scheme {
        ColorScheme::Light => DEFAULT_FG_LIGHT,
        ColorScheme::Dark => DEFAULT_FG_DARK,
    }
}

/// Fraction of the bg→fg ramp the panel surface lifts off `bg` (dark default
/// `#060606` → `#0d0d0d`).
const SURFACE_LIFT: f32 = 0.0314;
/// Raised-surface fraction (dark default → `#1e1e1e`).
const RAISED_LIFT: f32 = 0.1076;
/// Fraction of the fg→bg ramp muted text sinks toward `bg` (dark default
/// `#e5e5e5` → `#a1a1a1`).
const MUTED_SINK: f32 = 0.3049;
/// Faint-text fraction (dark default → `#737373`).
const FAINT_SINK: f32 = 0.5112;

// Mirror of the scheme currently installed as the theme global. Paint
// primitives ([`wash`], [`white_alpha`]) flip white↔black with the scheme so
// the ~130 call sites that pass a hardcoded white wash (hairlines, hovers,
// skeleton rows) keep painting the active scheme without threading `&Theme`
// through every element builder. Element builders all run on the UI thread;
// tests default to Dark and set the mirror explicitly when they need Light.
thread_local! {
    static CURRENT_SCHEME: Cell<ColorScheme> = const { Cell::new(ColorScheme::Dark) };
    /// The active theme's primary text color — for element builders that have
    /// no `&Theme` in scope (shared hover closures like [`ghost_hover`]).
    static CURRENT_TEXT: Cell<Hsla> = Cell::new(hsla(0.0, 0.0, 0.898, 1.0));
}

#[cfg(test)]
pub(crate) fn set_test_scheme(scheme: ColorScheme) {
    CURRENT_SCHEME.with(|cell| cell.set(scheme));
}

/// The app theme. One concrete instance per scheme; rebuilt when the user
/// changes appearance settings or the system scheme flips.
#[derive(Debug, Clone)]
pub struct Theme {
    /// The scheme this theme was built for — the direction of its neutral ramp.
    pub scheme: ColorScheme,
    // ---- paint: neutral surfaces (oklch chroma 0) ----
    /// App background — oklch(0.145 0 0) ≡ `#0a0a0a`.
    pub bg: Hsla,
    /// Panel / sidebar surface — one scale step up.
    pub surface: Hsla,
    /// Raised surface: popovers, dialogs, cards.
    pub surface_raised: Hsla,
    /// Hover wash for interactive rows/buttons (white, low alpha).
    pub element_hover: Hsla,
    /// Active/selected wash (white, slightly higher alpha).
    pub element_active: Hsla,
    /// Hairline border — white at low alpha.
    pub border: Hsla,
    /// Stronger border for focused/raised edges.
    pub border_strong: Hsla,

    // ---- paint: text ----
    /// Primary text.
    pub text: Hsla,
    /// Muted text: timestamps, secondary labels.
    pub text_muted: Hsla,
    /// Faint text: placeholders, disabled.
    pub text_faint: Hsla,

    // ---- paint: accents ----
    /// Accent — indigo (working indicator, links, selection tint).
    pub accent: Hsla,
    /// Stronger accent for fills.
    pub accent_strong: Hsla,
    /// Danger — red (errors, stop button).
    pub danger: Hsla,
    /// Warning — amber (offline notices, awaiting-input).
    pub warning: Hsla,

    // ---- fonts ----
    /// UI font family (bundling of Geist lands with asset work; until then the
    /// text system falls back to the system sans when the family is missing).
    pub font_sans: SharedString,
    /// Monospace family for code/terminal.
    pub font_mono: SharedString,
    /// Explicit system fallbacks, for callers that want to skip the lookup.
    pub font_sans_fallback: SharedString,
    pub font_mono_fallback: SharedString,
}

impl Theme {
    // ---- numbers drive layout (px) ----
    /// Frost translucency over the blurred window background (macOS vibrancy).
    /// Opaque elsewhere: Linux/Windows get no compositor-blur guarantee, and a
    /// merely transparent window would show raw desktop through the sidebar.
    /// Darkness matched by eye to a reference Electron app's dark glass. That
    /// scrim is 0.76 over `hsl(0 0% 3%)`, but it sits on Electron's
    /// `under-window` vibrancy MATERIAL, which pre-darkens the blur; our bare
    /// backdrop blur has no material layer, so the scrim runs heavier to land
    /// on the same perceived tone (see [`Theme::glass`]).
    pub const GLASS_ALPHA: f32 = if cfg!(target_os = "macos") { 0.90 } else { 1.0 };
    /// Main-panel header height (comet `h-11`) — in-card headers (changes pane).
    pub const HEADER_HEIGHT: f32 = 44.0;
    /// The unified window titlebar (traffic lights + cluster + tabs). Content
    /// rides [`Self::TITLEBAR_TOP_PAD`] lower than center so the air above
    /// matches the perceived gap to the inset card below (border + card body).
    pub const TITLEBAR_HEIGHT: f32 = 38.0;
    /// Downward shift of titlebar content within the bar.
    pub const TITLEBAR_TOP_PAD: f32 = 2.0;
    /// Reserved status strip under the content outlet (comet `h-6`) — the
    /// WorkingIndicator row; reserving it keeps the composer from shifting.
    pub const STATUS_STRIP_HEIGHT: f32 = 24.0;
    /// Message bubble corner radius.
    pub const BUBBLE_RADIUS: f32 = 16.0;
    /// Panel / card corner radius.
    pub const PANEL_RADIUS: f32 = 10.0;
    /// Small control radius (buttons, chips).
    pub const CONTROL_RADIUS: f32 = 6.0;
    /// Base spacing steps.
    pub const SPACE_XS: f32 = 4.0;
    pub const SPACE_SM: f32 = 8.0;
    pub const SPACE_MD: f32 = 12.0;
    pub const SPACE_LG: f32 = 16.0;

    /// The frost tint painted over the blurred window background (macOS
    /// glass). Dark keeps the reference near-black vibrancy scrim. Light is
    /// deliberately opaque: a pale translucent tint lets the wallpaper bleed
    /// through and turns otherwise predictable chrome into muddy grey.
    pub fn glass(&self) -> Hsla {
        match self.scheme {
            ColorScheme::Dark if Self::GLASS_ALPHA < 1.0 => grey(8).opacity(Self::GLASS_ALPHA),
            ColorScheme::Dark | ColorScheme::Light => self.surface,
        }
    }

    /// The floating-card tint over the frosted backdrop (popovers, dialogs,
    /// sidebar peek). Dark keeps its translucent glass; light elevates with an
    /// opaque plate plus border/shadow instead of lightness-through-wallpaper.
    pub fn glass_card(&self) -> Hsla {
        match self.scheme {
            ColorScheme::Dark if Self::GLASS_ALPHA < 1.0 => grey(0x16).opacity(0.65),
            ColorScheme::Dark => grey(0x16),
            ColorScheme::Light => self.bg,
        }
    }

    /// The composer and text-input plate. A faint white wash raises a dark
    /// surface; mirroring that to black in light mode makes the control look
    /// recessed and lets its drop-shadow plate show through. Light therefore
    /// uses an opaque background and lets border/shadow carry elevation.
    pub fn input_bg(&self) -> Hsla {
        match self.scheme {
            ColorScheme::Dark => hsla(0.0, 0.0, 1.0, 0.03),
            ColorScheme::Light => self.bg,
        }
    }

    /// Terminal surface. Unlike the previous always-dark island, light mode
    /// follows the app plane and pairs it with a dedicated dark ANSI palette.
    pub fn terminal_bg(&self) -> Hsla {
        match self.scheme {
            ColorScheme::Dark => grey(0x09),
            ColorScheme::Light => self.bg,
        }
    }

    pub fn terminal_cursor(&self) -> Hsla {
        self.text
            .opacity(if self.scheme.is_dark() { 0.35 } else { 0.55 })
    }

    /// Scheme-aware translucent wash (soft-white in dark, soft-black in light)
    /// — the theme-backed form of the [`wash`] primitive.
    pub fn wash(&self, alpha: f32) -> Hsla {
        crate::theme::wash(alpha)
    }

    /// Build a theme from the persisted recipe: the two anchor hexes and the
    /// contrast percentage. Every intermediate role is a fixed fraction of the
    /// distance between `bg` and `fg`, scaled by `contrast / 100` — so the
    /// defaults reproduce the pre-theming dark theme pixel-for-pixel, and a
    /// light theme is its exact mirror. Invalid hexes fall back to the
    /// scheme's defaults; accents are brand hues and stay fixed.
    pub fn custom(scheme: ColorScheme, bg_hex: &str, fg_hex: &str, contrast: f32) -> Self {
        let spread = (contrast / DEFAULT_CONTRAST).clamp(0.0, 1.0);
        let default_bg = default_bg_hex(scheme);
        let default_fg = default_fg_hex(scheme);
        let bg = parse_hex(bg_hex)
            .or_else(|| parse_hex(default_bg))
            .unwrap_or_else(|| {
                // Unreachable — the default hexes are valid; keep the app alive on
                // a broken constant.
                hsla(0.0, 0.0, 0.0, 1.0)
            });
        let fg = parse_hex(fg_hex)
            .or_else(|| parse_hex(default_fg))
            .unwrap_or_else(|| hsla(0.0, 0.0, 1.0, 1.0));
        // Wash/hairline poles: the scheme's light pole (white in dark, black in
        // light) so translucent overlays read on any surface.
        let wash_l = if scheme.is_dark() { 0.92 } else { 0.10 };
        let hairline_l = if scheme.is_dark() { 1.0 } else { 0.0 };
        // A one-pixel line needs more ink on white, while fill alphas carry
        // over unchanged. This is the useful role split from upstream's light
        // palette without giving up our custom color anchors.
        let hairline_scale = if scheme.is_dark() { 1.0 } else { 1.35 };
        let (accent, accent_strong, danger, warning) = match scheme {
            ColorScheme::Dark => (
                oklch(0.673, 0.182, 276.935), // indigo-400
                oklch(0.585, 0.233, 277.117), // indigo-500
                oklch(0.704, 0.191, 22.216),  // red-400
                oklch(0.828, 0.189, 84.429),  // amber-400
            ),
            ColorScheme::Light => (
                oklch(0.511, 0.262, 276.966), // indigo-600
                oklch(0.511, 0.262, 276.966), // indigo-600
                oklch(0.577, 0.245, 27.325),  // red-600
                oklch(0.555, 0.163, 48.998),  // amber-700
            ),
        };
        Self {
            scheme,
            bg,
            surface: mix(bg, fg, SURFACE_LIFT * spread),
            surface_raised: mix(bg, fg, RAISED_LIFT * spread),
            element_hover: hsla(0.0, 0.0, wash_l, 0.14 * spread),
            element_active: hsla(0.0, 0.0, wash_l, 0.16 * spread),
            border: hsla(
                0.0,
                0.0,
                hairline_l,
                (0.08 * spread * hairline_scale).min(0.5),
            ),
            border_strong: hsla(
                0.0,
                0.0,
                hairline_l,
                (0.14 * spread * hairline_scale).min(0.5),
            ),
            text: fg,
            text_muted: mix(fg, bg, MUTED_SINK * spread),
            text_faint: mix(fg, bg, FAINT_SINK * spread),
            accent,
            accent_strong,
            danger,
            warning,
            font_sans: "Geist".into(),
            font_mono: "Geist Mono".into(),
            font_sans_fallback: system_sans().into(),
            font_mono_fallback: system_mono().into(),
        }
    }

    /// Build the (only) theme. The surface tones are sampled straight from the
    /// reference screenshots of the original app (docs/reference): main panel
    /// `#060606`, shell/sidebar `#0d0d0d`.
    pub fn dark() -> Self {
        Self::custom(ColorScheme::Dark, "", "", DEFAULT_CONTRAST)
    }

    /// The light mirror: near-white surfaces, near-black text, dark hairlines.
    pub fn light() -> Self {
        Self::custom(ColorScheme::Light, "", "", DEFAULT_CONTRAST)
    }

    /// Keep the paint system fixed while replacing only the two text roles.
    /// Font family names are resolved by gpui and fall back to its platform
    /// stack when a saved family is not installed on this machine.
    pub fn with_fonts(
        mut self,
        ui_font: impl Into<SharedString>,
        code_font: impl Into<SharedString>,
    ) -> Self {
        self.font_sans = ui_font.into();
        self.font_mono = code_font.into();
        self
    }

    /// Read the theme global.
    pub fn of(cx: &App) -> &Theme {
        cx.global::<Theme>()
    }

    /// Install the theme as the gpui [`Global`] and sync the scheme mirror
    /// that drives [`wash`]/[`white_alpha`] and [`current_text`]. Always
    /// install through this — a bare `cx.set_global` leaves the mirror on the
    /// previous scheme.
    pub fn install(cx: &mut App, theme: Theme) {
        CURRENT_SCHEME.with(|cell| cell.set(theme.scheme));
        CURRENT_TEXT.with(|cell| cell.set(theme.text));
        cx.set_global(theme);
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::dark()
    }
}

impl Global for Theme {}

// ---------------------------------------------------------------------------
// Theme recipe (persisted appearance settings)
// ---------------------------------------------------------------------------

/// The persisted theme recipe — preference, custom color anchors, and the
/// two text roles. The shell keeps one as a global so the window-appearance
/// observer (which runs with only `&mut Window, &mut App` and cannot reach
/// the shell entity) can rebuild the theme when the OS scheme flips.
#[derive(Debug, Clone)]
pub struct ThemeConfig {
    pub preference: ThemePreference,
    /// Custom background hex; `None` uses the active scheme's default.
    pub bg_hex: Option<String>,
    /// Custom foreground hex; `None` uses the active scheme's default.
    pub fg_hex: Option<String>,
    /// Contrast percentage (0..100) — the tonal spread between the anchor hexes
    /// and the derived roles (surfaces, muted/faint text, hairlines, washes).
    pub contrast: f32,
    pub ui_font: String,
    pub code_font: String,
}

impl ThemeConfig {
    pub fn build(&self, system: ColorScheme) -> Theme {
        Theme::custom(
            self.preference.resolved(system),
            self.bg_hex.as_deref().unwrap_or_default(),
            self.fg_hex.as_deref().unwrap_or_default(),
            self.contrast,
        )
        .with_fonts(self.ui_font.clone(), self.code_font.clone())
    }

    /// The hex shown in settings for the background: the custom value, or the
    /// active scheme's default while untouched.
    pub fn effective_bg_hex(&self, scheme: ColorScheme) -> String {
        self.bg_hex
            .clone()
            .unwrap_or_else(|| default_bg_hex(scheme).to_string())
    }

    pub fn effective_fg_hex(&self, scheme: ColorScheme) -> String {
        self.fg_hex
            .clone()
            .unwrap_or_else(|| default_fg_hex(scheme).to_string())
    }
}

impl Global for ThemeConfig {}

fn system_sans() -> &'static str {
    if cfg!(target_os = "macos") {
        "Helvetica"
    } else if cfg!(target_os = "windows") {
        "Segoe UI"
    } else {
        "DejaVu Sans"
    }
}

fn system_mono() -> &'static str {
    if cfg!(target_os = "macos") {
        "Menlo"
    } else if cfg!(target_os = "windows") {
        "Consolas"
    } else {
        "DejaVu Sans Mono"
    }
}

/// A neutral (chroma 0) oklch tone as Hsla. Chroma 0 means r == g == b exactly,
/// so this goes straight to an achromatic Hsla (skipping the hue math avoids
/// float-noise saturation).
pub fn neutral(lightness: f32) -> Hsla {
    let [v, _, _] = oklch_to_srgb(lightness, 0.0, 0.0);
    hsla(0.0, 0.0, v, 1.0)
}

/// Interactive-state wash: TRANSLUCENT soft-white, with alphas high enough to
/// stay visible at the brightest backdrop the 0.90 glass scrim can produce
/// (~L 0.13 over pure white — a 12% wash still adds ~+24 luma there). Fully
/// opaque washes killed the glass and flashed dark mid-fade (user reports);
/// hover fades must rest on `wash(0.0)`, never transparent BLACK, so the
/// interpolation stays white-toned.
///
/// Scheme-aware: in a light theme the wash is the soft-BLACK mirror (l 0.10)
/// so hover states keep painting on light surfaces.
pub fn wash(alpha: f32) -> Hsla {
    let l = CURRENT_SCHEME.with(|s| if s.get().is_dark() { 0.92 } else { 0.10 });
    hsla(0.0, 0.0, l, alpha)
}

/// The scheme's hairline/wash pole at the given alpha — white in dark themes,
/// black in light (so hairlines and translucent fills read on any surface).
pub fn white_alpha(alpha: f32) -> Hsla {
    let l = CURRENT_SCHEME.with(|s| if s.get().is_dark() { 1.0 } else { 0.0 });
    hsla(0.0, 0.0, l, alpha)
}

/// The active theme's primary text color — for builders without a `&Theme`.
pub fn current_text() -> Hsla {
    CURRENT_TEXT.with(|cell| cell.get())
}

/// The active theme's scheme — for paint primitives without a `&Theme`.
pub fn current_scheme() -> ColorScheme {
    CURRENT_SCHEME.with(|cell| cell.get())
}

/// Parse a `#RRGGBB` hex (leading `#` optional, case-insensitive).
pub fn parse_hex(text: &str) -> Option<Hsla> {
    let hex = text.trim().trim_start_matches('#');
    if hex.len() != 6 {
        return None;
    }
    let mut channels = [0u8; 3];
    for (i, byte) in hex.bytes().enumerate() {
        let nibble = (byte as char).to_digit(16)?;
        channels[i / 2] = (channels[i / 2] << 4) | nibble as u8;
    }
    let [r, g, b] = channels;
    Some(Hsla::from(Rgba {
        r: r as f32 / 255.0,
        g: g as f32 / 255.0,
        b: b as f32 / 255.0,
        a: 1.0,
    }))
}

/// The hex string (`#rrggbb`) for an opaque color — the display form for the
/// appearance settings.
pub fn hex_of(color: Hsla) -> String {
    let rgba = Rgba::from(color);
    format!(
        "#{:02x}{:02x}{:02x}",
        (rgba.r * 255.0).round().clamp(0.0, 255.0) as u8,
        (rgba.g * 255.0).round().clamp(0.0, 255.0) as u8,
        (rgba.b * 255.0).round().clamp(0.0, 255.0) as u8,
    )
}

/// Selected-state glass treatment (tabs, session rows, space rows): a
/// quiet lift that separates the row without turning it into a solid card.
/// Keep selection below hover's visual weight; the brighter title carries the
/// rest of the affordance.
pub fn glass_selected_bg() -> Hsla {
    wash(0.07)
}

/// The context gauge's five states, lit-cell count → color: a battery drains
/// green → lime → amber → orange → red. The only hue ramp in a monochrome
/// app, and it earns the exception by being a MEASUREMENT — the reading is
/// the color, so it has to be legible at a glance and at 10px.
pub fn context_ramp(level: u8) -> Hsla {
    match level {
        5 => oklch(0.792, 0.209, 151.711), // green-400
        4 => oklch(0.841, 0.238, 128.850), // lime-400
        3 => oklch(0.828, 0.189, 84.429),  // amber-400
        2 => oklch(0.750, 0.183, 55.934),  // orange-400
        _ => oklch(0.704, 0.191, 22.216),  // red-400
    }
}

/// The selected chip's bright outline, as an INSET shadow: gpui paints inset
/// shadows ON TOP of the background, edges only — a border with zero layout
/// cost. Drop shadows are filled rects painted BEHIND the element, and behind
/// a 5% fill they showed straight through as an opaque dark plate with a
/// greyed ring (user report) — nothing may paint behind a glass chip.
/// A restrained inset edge for selected rows. It should clarify the active
/// surface without reading as a button outline.
pub fn glass_selected_shadows() -> Vec<gpui::BoxShadow> {
    vec![gpui::BoxShadow {
        color: white_alpha(0.04),
        offset: gpui::point(gpui::px(0.0), gpui::px(0.0)),
        blur_radius: gpui::px(0.0),
        spread_radius: gpui::px(1.0),
        inset: true,
    }]
}

/// An exact achromatic tone from an 8-bit channel value (`grey(13)` ≡ `#0d0d0d`)
/// — for surfaces matched against reference-screenshot samples.
pub fn grey(value: u8) -> Hsla {
    hsla(0.0, 0.0, value as f32 / 255.0, 1.0)
}

/// Convert an oklch color (CSS notation: L 0..1, C, H in degrees) to gpui Hsla.
pub fn oklch(l: f32, c: f32, h_deg: f32) -> Hsla {
    let [r, g, b] = oklch_to_srgb(l, c, h_deg);
    let (h, s, l) = rgb_to_hsl(r, g, b);
    hsla(h, s, l, 1.0)
}

/// oklch → sRGB (each 0..1, clamped/gamut-clipped per channel).
/// Reference: Björn Ottosson's OKLab definition (the same matrices CSS Color 4 uses).
pub(crate) fn oklch_to_srgb(l: f32, c: f32, h_deg: f32) -> [f32; 3] {
    let h = h_deg.to_radians();
    let a = c * h.cos();
    let b = c * h.sin();

    // OKLab → LMS (cube roots undone)
    let l_ = l + 0.396_337_78 * a + 0.215_803_76 * b;
    let m_ = l - 0.105_561_346 * a - 0.063_854_17 * b;
    let s_ = l - 0.089_484_18 * a - 1.291_485_5 * b;
    let (l3, m3, s3) = (l_ * l_ * l_, m_ * m_ * m_, s_ * s_ * s_);

    // LMS → linear sRGB
    let r = 4.076_741_7 * l3 - 3.307_711_6 * m3 + 0.230_969_93 * s3;
    let g = -1.268_438 * l3 + 2.609_757_4 * m3 - 0.341_319_4 * s3;
    let b = -0.004_196_086_3 * l3 - 0.703_418_6 * m3 + 1.707_614_7 * s3;

    [gamma_encode(r), gamma_encode(g), gamma_encode(b)]
}

fn gamma_encode(x: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    if x <= 0.003_130_8 {
        12.92 * x
    } else {
        1.055 * x.powf(1.0 / 2.4) - 0.055
    }
}

/// sRGB (0..1 components) → HSL, all components 0..1 (gpui's Hsla convention).
pub(crate) fn rgb_to_hsl(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    let delta = max - min;
    if delta < f32::EPSILON {
        return (0.0, 0.0, l);
    }
    let s = if l > 0.5 {
        delta / (2.0 - max - min)
    } else {
        delta / (max + min)
    };
    let h = if (max - r).abs() < f32::EPSILON {
        ((g - b) / delta).rem_euclid(6.0)
    } else if (max - g).abs() < f32::EPSILON {
        (b - r) / delta + 2.0
    } else {
        (r - g) / delta + 4.0
    } / 6.0;
    (h, s, l)
}

/// Linear per-component mix of two colors (paint helper for the gradient spinner).
pub fn mix(a: Hsla, b: Hsla, t: f32) -> Hsla {
    let t = t.clamp(0.0, 1.0);
    let lerp = |x: f32, y: f32| x + (y - x) * t;
    // Mix through hue naively — both spinner endpoints sit close enough on the
    // wheel that shortest-arc handling isn't needed for our palette.
    hsla(
        lerp(a.h, b.h),
        lerp(a.s, b.s),
        lerp(a.l, b.l),
        lerp(a.a, b.a),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn srgb_u8(c: [f32; 3]) -> [u8; 3] {
        [
            (c[0] * 255.0).round() as u8,
            (c[1] * 255.0).round() as u8,
            (c[2] * 255.0).round() as u8,
        ]
    }

    #[test]
    fn neutral_950_is_0a0a0a() {
        // oklch(0.145 0 0) is Tailwind neutral-950, comet's app background.
        let rgb = srgb_u8(oklch_to_srgb(0.145, 0.0, 0.0));
        assert_eq!(rgb, [10, 10, 10]);
    }

    #[test]
    fn oklch_accents_match_reference() {
        // Reference values computed independently (CSS Color 4 matrices).
        assert_eq!(
            srgb_u8(oklch_to_srgb(0.673, 0.182, 276.935)),
            [124, 134, 255]
        ); // indigo-400
        assert_eq!(
            srgb_u8(oklch_to_srgb(0.704, 0.191, 22.216)),
            [255, 100, 103]
        ); // red-400
        assert_eq!(srgb_u8(oklch_to_srgb(0.828, 0.189, 84.429)), [255, 185, 0]); // amber-400
    }

    #[test]
    fn neutral_scale_is_ordered() {
        let t = Theme::dark();
        assert!(t.bg.l < t.surface.l);
        assert!(t.surface.l < t.surface_raised.l);
        assert!(t.surface_raised.l < t.text_faint.l);
        assert!(t.text_faint.l < t.text_muted.l);
        assert!(t.text_muted.l < t.text.l);
        // Monochrome: neutrals carry no saturation.
        for c in [
            t.bg,
            t.surface,
            t.surface_raised,
            t.text,
            t.text_muted,
            t.text_faint,
        ] {
            assert_eq!(c.s, 0.0);
            assert_eq!(c.a, 1.0);
        }
    }

    #[test]
    fn hairlines_are_white_and_washes_are_mid_grey() {
        let t = Theme::dark();
        // Hairlines stay white — they only need to read on dark surfaces.
        for c in [t.border, t.border_strong] {
            assert_eq!(c.l, 1.0, "hairlines are white");
            assert!(c.a > 0.0 && c.a < 0.25, "low alpha, got {}", c.a);
        }
        // Washes are translucent soft-white with enough alpha to read at the
        // glass scrim's brightness ceiling.
        for c in [t.element_hover, t.element_active] {
            assert_eq!(c.l, 0.92, "washes are soft-white");
            assert!(c.a >= 0.05 && c.a < 0.35, "alpha in band, got {}", c.a);
        }
        assert!(t.border.a < t.border_strong.a);
        // Hover intentionally equals the active fill (selection differs by
        // its ring, not brightness — user request).
        assert!(t.element_hover.a <= t.element_active.a);
    }

    #[test]
    fn accent_hues_land_in_their_bands() {
        let t = Theme::dark();
        // Hsla hue is 0..1 of the wheel. Indigo ≈ 230-250°, red < 15°, amber ≈ 40-55°.
        let deg = |c: Hsla| c.h * 360.0;
        assert!(
            (215.0..265.0).contains(&deg(t.accent)),
            "indigo hue {}",
            deg(t.accent)
        );
        assert!(
            deg(t.danger) < 15.0 || deg(t.danger) > 345.0,
            "red hue {}",
            deg(t.danger)
        );
        assert!(
            (35.0..60.0).contains(&deg(t.warning)),
            "amber hue {}",
            deg(t.warning)
        );
    }

    #[test]
    fn mix_endpoints_and_midpoint() {
        let a = hsla(0.0, 0.0, 0.0, 1.0);
        let b = hsla(0.5, 1.0, 1.0, 0.0);
        assert_eq!(mix(a, b, 0.0), a);
        assert_eq!(mix(a, b, 1.0), b);
        let mid = mix(a, b, 0.5);
        assert!((mid.l - 0.5).abs() < 1e-6 && (mid.a - 0.5).abs() < 1e-6);
        // Out-of-range t clamps.
        assert_eq!(mix(a, b, 2.0), b);
    }

    #[test]
    fn layout_numbers_match_comet() {
        assert_eq!(Theme::HEADER_HEIGHT, 44.0); // h-11
        assert_eq!(Theme::STATUS_STRIP_HEIGHT, 24.0); // h-6
        assert_eq!(Theme::BUBBLE_RADIUS, 16.0);
    }

    #[test]
    fn custom_defaults_reproduce_the_dark_theme() {
        let built = Theme::custom(ColorScheme::Dark, "#060606", "#e5e5e5", 100.0);
        let reference = Theme::dark();
        assert_eq!(built.scheme, ColorScheme::Dark);
        // Anchor hexes land exactly.
        assert!((built.bg.l - reference.bg.l).abs() < 1e-3);
        assert!((built.text.l - reference.text.l).abs() < 1e-3);
        // Derived roles reproduce the pre-theming tones (srgb 13/30/161/115).
        for (name, got, want) in [
            ("surface", built.surface.l, reference.surface.l),
            (
                "surface_raised",
                built.surface_raised.l,
                reference.surface_raised.l,
            ),
            ("text_muted", built.text_muted.l, reference.text_muted.l),
            ("text_faint", built.text_faint.l, reference.text_faint.l),
        ] {
            assert!((got - want).abs() < 1e-3, "{name}: {got} vs {want}");
        }
        // Hairlines stay white-pole, washes soft-white-pole in dark.
        assert_eq!(built.border.l, 1.0);
        assert_eq!(built.element_hover.l, 0.92);
    }

    #[test]
    fn light_scheme_mirrors_the_ramp() {
        let t = Theme::light();
        assert_eq!(t.scheme, ColorScheme::Light);
        // Light ramp: text darker than bg, surfaces between the two and
        // raised surfaces pushed further toward the text pole.
        assert!(t.text.l < t.bg.l, "dark text on light bg");
        assert!(t.text.l < t.surface.l && t.surface.l < t.bg.l);
        assert!(
            t.surface_raised.l < t.surface.l,
            "raised is darker in light"
        );
        // Muted/faint sit between text and bg, each closer to bg than the
        // brighter role (in light they're LIGHTER than text, in dark darker).
        let toward_bg = |c: Hsla| (c.l - t.bg.l).abs();
        assert!(
            toward_bg(t.text_muted) < toward_bg(t.text),
            "muted is dimmer"
        );
        assert!(toward_bg(t.text_faint) < toward_bg(t.text_muted));
        assert!(toward_bg(t.text_muted) > 0.0 && toward_bg(t.text_faint) > 0.0);
        // Hairlines/washes flip to the dark pole.
        assert_eq!(t.border.l, 0.0);
        assert_eq!(t.element_hover.l, 0.10);
    }

    #[test]
    fn contrast_zero_flattens_the_ramp() {
        let flat = Theme::custom(ColorScheme::Dark, "#060606", "#e5e5e5", 0.0);
        // No tonal spread: surfaces collapse to bg, secondary text to fg,
        // hairline washes to fully transparent.
        assert!((flat.surface.l - flat.bg.l).abs() < 1e-6);
        assert!((flat.surface_raised.l - flat.bg.l).abs() < 1e-6);
        assert!((flat.text_muted.l - flat.text.l).abs() < 1e-6);
        assert!((flat.text_faint.l - flat.text.l).abs() < 1e-6);
        assert_eq!(flat.border.a, 0.0);
        assert_eq!(flat.element_hover.a, 0.0);
    }

    #[test]
    fn hex_parsing_and_display_round_trip() {
        for hex in ["#a1b2c3", "#000000", "#ffffff", "#0d0d0d", "#ABCDEF"] {
            let color = parse_hex(hex).unwrap_or_else(|| panic!("parse {hex}"));
            assert_eq!(
                hex_of(color).to_lowercase(),
                hex.to_lowercase(),
                "round-trip"
            );
        }
        assert!(parse_hex("abc").is_none());
        assert!(parse_hex("#12345g").is_none());
        assert!(parse_hex("").is_none());
        assert!(parse_hex("#ffffff00").is_none(), "no 8-digit hex");
        // Scheme defaults parse.
        assert!(parse_hex(default_bg_hex(ColorScheme::Dark)).is_some());
        assert!(parse_hex(default_fg_hex(ColorScheme::Light)).is_some());
    }

    #[test]
    fn invalid_custom_hexes_fall_back_to_scheme_defaults() {
        let t = Theme::custom(ColorScheme::Dark, "not-a-hex", "#zzzzzz", 100.0);
        assert!((t.bg.l - parse_hex(DEFAULT_BG_DARK).unwrap().l).abs() < 1e-6);
        assert!((t.text.l - parse_hex(DEFAULT_FG_DARK).unwrap().l).abs() < 1e-6);
    }

    #[test]
    fn preference_resolves_system() {
        let light = ColorScheme::Light;
        let dark = ColorScheme::Dark;
        assert_eq!(ThemePreference::System.resolved(light), light);
        assert_eq!(ThemePreference::System.resolved(dark), dark);
        assert_eq!(ThemePreference::Light.resolved(dark), light);
        assert_eq!(ThemePreference::Dark.resolved(light), dark);
        assert_eq!(ThemePreference::System.label(), "System");
    }

    #[test]
    fn paint_primitives_flip_with_the_scheme_mirror() {
        // Default (Dark) mirrors the pre-theming white washes.
        assert_eq!(white_alpha(0.1).l, 1.0);
        assert_eq!(wash(0.1).l, 0.92);
        set_test_scheme(ColorScheme::Light);
        assert_eq!(white_alpha(0.1).l, 0.0, "black hairline in light");
        assert_eq!(wash(0.1).l, 0.10, "soft-black wash in light");
        set_test_scheme(ColorScheme::Dark);
    }

    #[test]
    fn glass_card_mirrors_the_scheme() {
        let dark = Theme::dark();
        let light = Theme::light();
        // Dark card tone ~#161616; light card is an opaque near-white plate.
        let (dl, light_card) = (dark.glass_card().l, light.glass_card());
        assert!((dl - 0x16 as f32 / 255.0).abs() < 1e-3, "dark {dl}");
        assert!(light_card.l > 0.9, "light {}", light_card.l);
        assert_eq!(light_card.a, 1.0);
        assert_eq!(
            light.glass().a,
            1.0,
            "light chrome does not sample wallpaper"
        );
    }

    #[test]
    fn light_inputs_are_opaque_raised_plates() {
        let dark = Theme::dark();
        let light = Theme::light();
        assert!(dark.input_bg().a < 1.0, "dark keeps its translucent lift");
        assert_eq!(light.input_bg(), light.bg);
        assert_eq!(
            light.input_bg().a,
            1.0,
            "shadow cannot bleed through the plate"
        );
    }
}
