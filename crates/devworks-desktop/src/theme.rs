//! The approved warm neutral visual language (UI-15, revision 08): one palette and one set of
//! spacing and stroke rules shared by every screen. Values follow the prototype's design tokens
//! (`docs/planning/workbench.css`); the prototype stays a design reference only.
use eframe::egui::{
    self, Color32, CornerRadius, FontFamily, FontId, Margin, Stroke, TextStyle, Visuals,
};

pub const BG: Color32 = Color32::from_rgb(0x1c, 0x1d, 0x20);
pub const PANEL: Color32 = Color32::from_rgb(0x24, 0x25, 0x29);
pub const RAISED: Color32 = Color32::from_rgb(0x2d, 0x2e, 0x33);
pub const HOVER: Color32 = Color32::from_rgb(0x34, 0x36, 0x3c);
pub const LINE: Color32 = Color32::from_rgb(0x41, 0x43, 0x49);
pub const SOFTLINE: Color32 = Color32::from_rgb(0x34, 0x36, 0x3b);
pub const TEXT: Color32 = Color32::from_rgb(0xee, 0xea, 0xe3);
pub const MUTED: Color32 = Color32::from_rgb(0xb7, 0xb5, 0xb0);
pub const DIM: Color32 = Color32::from_rgb(0xaa, 0xa9, 0xa6);
pub const ACCENT: Color32 = Color32::from_rgb(0xb6, 0xc9, 0xf0);
pub const ACCENT_BG: Color32 = Color32::from_rgb(0x30, 0x3c, 0x52);
pub const GREEN: Color32 = Color32::from_rgb(0xa7, 0xd6, 0xbd);
pub const GREEN_BG: Color32 = Color32::from_rgb(0x29, 0x3e, 0x33);
pub const AMBER: Color32 = Color32::from_rgb(0xed, 0xc8, 0x8b);
pub const AMBER_BG: Color32 = Color32::from_rgb(0x44, 0x3b, 0x2b);
pub const RED: Color32 = Color32::from_rgb(0xf0, 0xa8, 0xa0);
pub const RED_BG: Color32 = Color32::from_rgb(0x45, 0x2b, 0x2b);

/// Corner radius of bordered groups and panes.
pub const RADIUS: u8 = 6;
/// Outer gutter around page panes.
pub const GUTTER: f32 = 20.0;
/// Width of the right inner sidebars and the Home Current / Chat History sidebar.
pub const SIDEBAR: f32 = 320.0;
/// Width of the left navigation.
pub const NAV: f32 = 206.0;
/// Below this content width, inner sidebars stack or overlay instead of sitting alongside.
pub const NARROW: f32 = 760.0;

pub fn line() -> Stroke {
    Stroke::new(1.0, LINE)
}
pub fn soft_line() -> Stroke {
    Stroke::new(1.0, SOFTLINE)
}
pub fn radius() -> CornerRadius {
    CornerRadius::same(RADIUS)
}
pub fn margin(x: i8, y: i8) -> Margin {
    Margin::symmetric(x, y)
}

/// Applies the palette, type scale and spacing to every theme of `ctx`.
pub fn apply(ctx: &egui::Context) {
    ctx.set_theme(egui::ThemePreference::Dark);
    ctx.all_styles_mut(|style| {
        let mut v = Visuals::dark();
        v.panel_fill = BG;
        v.window_fill = PANEL;
        v.window_stroke = line();
        v.window_corner_radius = radius();
        v.extreme_bg_color = Color32::from_rgb(0x19, 0x1a, 0x1d);
        v.faint_bg_color = PANEL;
        v.code_bg_color = RAISED;
        v.hyperlink_color = ACCENT;
        v.warn_fg_color = AMBER;
        v.error_fg_color = RED;
        v.selection.bg_fill = ACCENT_BG;
        v.selection.stroke = Stroke::new(1.0, ACCENT);
        v.widgets.noninteractive.bg_fill = PANEL;
        v.widgets.noninteractive.weak_bg_fill = PANEL;
        v.widgets.noninteractive.bg_stroke = line();
        v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
        v.widgets.noninteractive.corner_radius = radius();
        for (w, fill) in [
            (&mut v.widgets.inactive, RAISED),
            (&mut v.widgets.hovered, HOVER),
            (&mut v.widgets.active, HOVER),
            (&mut v.widgets.open, HOVER),
        ] {
            w.bg_fill = fill;
            w.weak_bg_fill = fill;
            w.corner_radius = CornerRadius::same(5);
            w.fg_stroke = Stroke::new(1.0, TEXT);
        }
        v.widgets.inactive.bg_stroke = line();
        v.widgets.hovered.bg_stroke = Stroke::new(1.0, MUTED);
        // A readable focus ring (UI-07): keyboard focus is drawn with the accent stroke.
        v.widgets.active.bg_stroke = Stroke::new(1.5, ACCENT);
        style.visuals = v;
        style.text_styles = [
            (
                TextStyle::Heading,
                FontId::new(20.0, FontFamily::Proportional),
            ),
            (TextStyle::Body, FontId::new(13.5, FontFamily::Proportional)),
            (
                TextStyle::Button,
                FontId::new(13.0, FontFamily::Proportional),
            ),
            (
                TextStyle::Small,
                FontId::new(11.5, FontFamily::Proportional),
            ),
            (
                TextStyle::Monospace,
                FontId::new(12.5, FontFamily::Monospace),
            ),
        ]
        .into();
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(10.0, 5.0);
        style.spacing.interact_size.y = 26.0;
    });
}
