//! Chat list rail.

use crate::models::Chat;
use crate::services::export::Format;
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
    TogglePin(Uuid),
    ToggleArchive(Uuid),
    Export(Uuid, Format),
    Rename(Uuid, String),
}

#[derive(Default)]
pub struct SidebarState {
    pub search_text: String,
    /// Chat id currently being renamed (if any) and its draft title.
    pub renaming: Option<(Uuid, String)>,
    /// Show archived chats in the list.
    pub show_archived: bool,
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
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            ui.checkbox(&mut state.show_archived, "show archived");
        });

        ui.add_space(6.0);
        ui.separator();

        ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for chat in chats {
                    if !state.show_archived && chat.archived {
                        continue;
                    }
                    render_row(ui, state, chat, active, &mut action);
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

fn render_row(
    ui: &mut Ui,
    state: &mut SidebarState,
    chat: &Chat,
    active: Option<Uuid>,
    action: &mut SidebarAction,
) {
    let selected = active == Some(chat.id);

    let is_renaming = matches!(&state.renaming, Some((id, _)) if *id == chat.id);
    if is_renaming {
        let mut commit_text: Option<String> = None;
        let mut cancel = false;
        if let Some((_, draft)) = state.renaming.as_mut() {
            ui.horizontal(|ui| {
                ui.add_space(8.0);
                let resp = ui.add_sized(
                    [180.0, 24.0],
                    TextEdit::singleline(draft).hint_text("new title"),
                );
                let enter_pressed =
                    resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if enter_pressed || ui.small_button("✓").clicked() {
                    commit_text = Some(draft.trim().to_string());
                } else if ui.small_button("✕").clicked() {
                    cancel = true;
                }
            });
        }
        if let Some(text) = commit_text {
            *action = SidebarAction::Rename(chat.id, text);
            state.renaming = None;
        } else if cancel {
            state.renaming = None;
        }
        return;
    }

    let row = ui.allocate_response(egui::vec2(ui.available_width(), 50.0), egui::Sense::click());
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
    let prefix = if chat.pinned { "📌 " } else { "" };
    let archived_suffix = if chat.archived { "  ·  archived" } else { "" };
    painter.text(
        inner.left_top(),
        egui::Align2::LEFT_TOP,
        format!("{prefix}{}{archived_suffix}", truncate(&chat.title, 30)),
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
        *action = SidebarAction::Select(chat.id);
    }
    row.context_menu(|ui| {
        if ui.button("Rename").clicked() {
            state.renaming = Some((chat.id, chat.title.clone()));
            ui.close();
        }
        if ui
            .button(if chat.pinned { "Unpin" } else { "Pin" })
            .clicked()
        {
            *action = SidebarAction::TogglePin(chat.id);
            ui.close();
        }
        if ui
            .button(if chat.archived {
                "Unarchive"
            } else {
                "Archive"
            })
            .clicked()
        {
            *action = SidebarAction::ToggleArchive(chat.id);
            ui.close();
        }
        ui.menu_button("Export…", |ui| {
            if ui.button("Markdown (.md)").clicked() {
                *action = SidebarAction::Export(chat.id, Format::Markdown);
                ui.close();
            }
            if ui.button("Obsidian (.md)").clicked() {
                *action = SidebarAction::Export(chat.id, Format::Obsidian);
                ui.close();
            }
            if ui.button("JSON (.json)").clicked() {
                *action = SidebarAction::Export(chat.id, Format::Json);
                ui.close();
            }
        });
        ui.separator();
        if ui
            .button(RichText::new("Delete").color(Color32::from_rgb(220, 100, 100)))
            .clicked()
        {
            *action = SidebarAction::Delete(chat.id);
            ui.close();
        }
    });
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
