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
    // Title line. The previous version allocated a fresh String per row
    // per frame via `format!("{prefix}{title}{suffix}", ...)`. egui's
    // `painter.text` already handles its own layout pass, so we build
    // the assembled title once into a stack-friendly buffer — but only
    // when prefix/suffix actually contribute. The hot path (no pin, no
    // archive) reuses the `Cow::Borrowed` returned by `truncate()`
    // with zero allocation.
    let title = truncate(&chat.title, 30);
    let title_str: std::borrow::Cow<'_, str> = match (chat.pinned, chat.archived) {
        (false, false) => title,
        (pinned, archived) => {
            let prefix = if pinned { "📌 " } else { "" };
            let suffix = if archived { "  ·  archived" } else { "" };
            let mut out = String::with_capacity(prefix.len() + title.len() + suffix.len());
            out.push_str(prefix);
            out.push_str(&title);
            out.push_str(suffix);
            std::borrow::Cow::Owned(out)
        }
    };
    painter.text(
        inner.left_top(),
        egui::Align2::LEFT_TOP,
        title_str.as_ref(),
        egui::TextStyle::Body.resolve(ui.style()),
        title_color,
    );
    // Subtitle: `provider · model`. Was a `format!()` per row per frame.
    // Pre-size the buffer to avoid the default doubling reallocs.
    let mut subtitle = String::with_capacity(chat.provider.len() + chat.model.len() + " · ".len());
    subtitle.push_str(&chat.provider);
    subtitle.push_str(" · ");
    subtitle.push_str(&chat.model);
    painter.text(
        inner.left_bottom() - egui::vec2(0.0, 4.0),
        egui::Align2::LEFT_BOTTOM,
        subtitle,
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

/// Truncate `s` to at most `max` Unicode characters, appending '…' when
/// truncated. Returns a `Cow` so the common short-title case is
/// allocation-free — the per-frame sidebar render path is on the UI hot
/// loop and this used to be `s.chars().count()` (full scan) followed by
/// `s.chars().take(max-1).collect()` (another full scan + allocation),
/// burning two scans on every title that didn't even need truncating.
///
/// Single-pass: `char_indices().nth(max)` gives the byte offset of the
/// `(max+1)`-th character if it exists. If it doesn't, the string is at
/// most `max` characters and we return the borrow. If it does, we slice
/// at the offset of the `max-1`-th character and append '…' once.
fn truncate(s: &str, max: usize) -> std::borrow::Cow<'_, str> {
    debug_assert!(max >= 1, "truncate max must be >= 1");
    // `char_indices().nth(max)` is None ⇔ s has ≤ max chars.
    if s.char_indices().nth(max).is_none() {
        return std::borrow::Cow::Borrowed(s);
    }
    // Find the byte offset to slice at: max-1 chars, then append '…'.
    let cut = s
        .char_indices()
        .nth(max.saturating_sub(1))
        .map_or(s.len(), |(idx, _)| idx);
    let mut out = String::with_capacity(cut + '…'.len_utf8());
    out.push_str(&s[..cut]);
    out.push('…');
    std::borrow::Cow::Owned(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::truncate;
    use std::borrow::Cow;

    #[test]
    fn truncate_short_strings_borrow_without_allocating() {
        // Borrowed variant means zero allocations on the hot path.
        let s = "short";
        let out = truncate(s, 30);
        assert!(matches!(out, Cow::Borrowed(_)));
        assert_eq!(out, "short");
    }

    #[test]
    fn truncate_exact_length_borrows() {
        let s = "abcdefghij"; // exactly 10
        let out = truncate(s, 10);
        assert!(matches!(out, Cow::Borrowed(_)));
        assert_eq!(out, "abcdefghij");
    }

    #[test]
    fn truncate_overlong_strings_clamp_with_ellipsis() {
        let out = truncate("abcdefghij", 5);
        assert!(matches!(out, Cow::Owned(_)));
        assert_eq!(out, "abcd…");
    }

    #[test]
    fn truncate_handles_multibyte_codepoints() {
        // Each emoji is a single Unicode char but multi-byte in UTF-8.
        // The cut point must land on a char boundary, not a byte one.
        let s = "🦀🦀🦀🦀🦀";
        let out = truncate(s, 3);
        assert_eq!(out, "🦀🦀…");
    }

    #[test]
    fn truncate_handles_max_equal_to_chars_plus_one() {
        let s = "abc";
        let out = truncate(s, 4);
        assert!(matches!(out, Cow::Borrowed(_)));
        assert_eq!(out, "abc");
    }
}
