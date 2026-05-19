//! Chat list rail.

use crate::models::Chat;
use crate::theme;
use crate::ui::toast::Toaster;
use egui::{Color32, Layout, RichText, ScrollArea, Stroke, TextEdit, Ui};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub enum SidebarAction {
    None,
    Select(Uuid),
    Delete(Uuid),
    NewChat,
    Search(String),
}

#[derive(Default)]
pub struct SidebarState {
    pub search_text: String,
}

pub fn render(
    ui: &mut Ui,
    state: &mut SidebarState,
    chats: &[Chat],
    active: Option<Uuid>,
    _toaster: &mut Toaster,
) -> SidebarAction {
    let mut action = SidebarAction::None;
    ui.vertical(|ui| {
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            ui.heading(RichText::new("GrokInsane").color(theme::ACCENT));
        });
        ui.add_space(8.0);

        ui.horizontal(|ui| {
            ui.add_space(8.0);
            let new_btn = egui::Button::new(RichText::new("+ New chat").color(Color32::WHITE))
                .fill(theme::ACCENT.gamma_multiply(0.18))
                .stroke(Stroke::new(1.0, theme::ACCENT))
                .corner_radius(10);
            if ui.add_sized([200.0, 30.0], new_btn).clicked() {
                action = SidebarAction::NewChat;
            }
        });

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            let resp = ui.add_sized(
                [200.0, 26.0],
                TextEdit::singleline(&mut state.search_text).hint_text("Search history…"),
            );
            if resp.changed() {
                action = SidebarAction::Search(state.search_text.clone());
            }
        });

        ui.add_space(6.0);
        ui.separator();

        ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for chat in chats {
                    let selected = active == Some(chat.id);
                    let row = ui.allocate_response(
                        egui::vec2(ui.available_width(), 50.0),
                        egui::Sense::click(),
                    );
                    let visuals = ui.visuals().clone();
                    let bg = if selected {
                        theme::ACCENT.gamma_multiply(0.18)
                    } else if row.hovered() {
                        visuals.widgets.hovered.bg_fill
                    } else {
                        Color32::TRANSPARENT
                    };
                    let painter = ui.painter_at(row.rect);
                    painter.rect_filled(row.rect, egui::CornerRadius::same(8), bg);
                    let inner = row.rect.shrink2(egui::vec2(10.0, 6.0));
                    let title_color = if selected {
                        Color32::WHITE
                    } else {
                        Color32::from_rgb(220, 225, 230)
                    };
                    painter.text(
                        inner.left_top(),
                        egui::Align2::LEFT_TOP,
                        truncate(&chat.title, 32),
                        egui::TextStyle::Body.resolve(ui.style()),
                        title_color,
                    );
                    painter.text(
                        inner.left_bottom() - egui::vec2(0.0, 4.0),
                        egui::Align2::LEFT_BOTTOM,
                        format!("{} · {}", chat.provider, chat.model),
                        egui::TextStyle::Small.resolve(ui.style()),
                        Color32::from_rgb(150, 158, 168),
                    );
                    if row.clicked() {
                        action = SidebarAction::Select(chat.id);
                    }
                    row.context_menu(|ui| {
                        if ui.button("Delete").clicked() {
                            action = SidebarAction::Delete(chat.id);
                            ui.close();
                        }
                    });
                }
            });

        ui.with_layout(Layout::bottom_up(egui::Align::LEFT), |ui| {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.add_space(8.0);
                ui.label(
                    RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
                        .small()
                        .color(Color32::from_rgb(120, 130, 140)),
                );
            });
        });
    });
    action
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max - 1).collect();
        out.push('…');
        out
    }
}
