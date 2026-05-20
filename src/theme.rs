//! Custom egui themes. The default "cosmic" palette is dark with neon green
//! accents inspired by xAI's brand; light + classic dark fall-backs are
//! available for users who prefer them.

use crate::models::ThemeMode;
use egui::{
    Color32, CornerRadius, FontFamily, FontId, Margin, Stroke, Style, TextStyle, Vec2, Visuals,
};

pub const ACCENT: Color32 = Color32::from_rgb(120, 255, 175);
pub const ACCENT_HOT: Color32 = Color32::from_rgb(170, 255, 200);
pub const SURFACE: Color32 = Color32::from_rgb(15, 18, 22);
pub const RAIL: Color32 = Color32::from_rgb(11, 13, 17);
pub const BORDER: Color32 = Color32::from_rgb(38, 44, 52);
pub const USER_BUBBLE: Color32 = Color32::from_rgb(36, 52, 46);
pub const ASSISTANT_BUBBLE: Color32 = Color32::from_rgb(22, 27, 33);

pub fn apply(ctx: &egui::Context, mode: ThemeMode, font_size: f32) {
    install_icon_font(ctx);
    let mut style = (*ctx.global_style()).clone();
    match mode {
        ThemeMode::Cosmic => apply_cosmic(&mut style),
        ThemeMode::Dark => style.visuals = Visuals::dark(),
        ThemeMode::Light => style.visuals = Visuals::light(),
    }
    apply_text_scale(&mut style, font_size);
    ctx.set_global_style(style);
}

/// Bundle the Phosphor icon font so we can use `egui_phosphor::regular::*`
/// glyphs anywhere a RichText is accepted. Installed once per context.
fn install_icon_font(ctx: &egui::Context) {
    use std::sync::OnceLock;
    static INSTALLED: OnceLock<()> = OnceLock::new();
    if INSTALLED.get().is_some() {
        return;
    }
    let mut fonts = egui::FontDefinitions::default();
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    ctx.set_fonts(fonts);
    let _ = INSTALLED.set(());
}

fn apply_cosmic(style: &mut Style) {
    let mut v = Visuals::dark();
    v.override_text_color = Some(Color32::from_rgb(220, 226, 230));
    v.panel_fill = SURFACE;
    v.window_fill = SURFACE;
    v.extreme_bg_color = RAIL;
    v.faint_bg_color = Color32::from_rgb(20, 25, 30);
    v.code_bg_color = Color32::from_rgb(13, 17, 22);
    v.window_stroke = Stroke::new(1.0, BORDER);
    v.window_corner_radius = CornerRadius::same(12);
    v.menu_corner_radius = CornerRadius::same(8);
    v.selection.bg_fill = ACCENT.gamma_multiply(0.35);
    v.selection.stroke = Stroke::new(1.0, ACCENT);
    v.hyperlink_color = ACCENT;
    v.widgets.noninteractive.bg_fill = SURFACE;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, Color32::from_rgb(170, 180, 190));
    v.widgets.inactive.bg_fill = Color32::from_rgb(24, 30, 36);
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, BORDER);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, Color32::from_rgb(200, 210, 220));
    v.widgets.inactive.corner_radius = CornerRadius::same(8);
    v.widgets.hovered.bg_fill = Color32::from_rgb(32, 40, 48);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, ACCENT.gamma_multiply(0.65));
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, ACCENT_HOT);
    v.widgets.hovered.corner_radius = CornerRadius::same(8);
    v.widgets.active.bg_fill = ACCENT.gamma_multiply(0.20);
    v.widgets.active.bg_stroke = Stroke::new(1.0, ACCENT);
    v.widgets.active.fg_stroke = Stroke::new(1.0, ACCENT_HOT);
    v.widgets.active.corner_radius = CornerRadius::same(8);
    style.visuals = v;
    style.spacing.item_spacing = Vec2::new(8.0, 6.0);
    style.spacing.window_margin = Margin::same(12);
    style.spacing.button_padding = Vec2::new(10.0, 6.0);
    style.spacing.scroll.bar_width = 8.0;
}

fn apply_text_scale(style: &mut Style, base: f32) {
    let body = base.max(10.0);
    style
        .text_styles
        .insert(TextStyle::Body, FontId::new(body, FontFamily::Proportional));
    style.text_styles.insert(
        TextStyle::Button,
        FontId::new(body, FontFamily::Proportional),
    );
    style.text_styles.insert(
        TextStyle::Monospace,
        FontId::new(body, FontFamily::Monospace),
    );
    style.text_styles.insert(
        TextStyle::Heading,
        FontId::new(body + 6.0, FontFamily::Proportional),
    );
    style.text_styles.insert(
        TextStyle::Small,
        FontId::new(body - 2.0, FontFamily::Proportional),
    );
}
