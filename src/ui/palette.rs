//! Cmd/Ctrl-K command palette.
//!
//! Surfaces every meaningful app action — new chat, open settings, toggle
//! voice/TTS, switch theme, switch provider, jump to a chat by title — behind
//! a single fuzzy-matched picker. The palette lives over the whole window
//! and dismisses on `Escape`, click-outside, or after running an action.

use crate::models::{Chat, Provider, ThemeMode};
use egui::{Align2, Color32, CornerRadius, Frame, Key, Margin, Order, RichText, Stroke, TextEdit};

/// Actions the palette can emit. The owning `App` consumes whatever comes
/// back and mutates state accordingly.
#[derive(Debug, Clone)]
pub enum PaletteAction {
    None,
    NewChat,
    OpenSettings,
    ToggleVoice,
    ToggleTts,
    ToggleRag,
    Theme(ThemeMode),
    Provider(Provider),
    SelectChat(uuid::Uuid),
    ExportActiveChat,
    Quit,
}

#[derive(Default)]
pub struct PaletteState {
    pub open: bool,
    pub query: String,
    pub cursor: usize,
}

impl PaletteState {
    pub fn open(&mut self) {
        self.open = true;
        self.query.clear();
        self.cursor = 0;
    }

    #[allow(dead_code)] // public escape hatch; the palette closes itself otherwise
    pub fn close(&mut self) {
        self.open = false;
    }
}

/// One entry in the palette. We build the list each render — there are only a
/// few dozen items even with chats included, so it's free.
struct Entry {
    title: String,
    subtitle: String,
    action: PaletteAction,
    /// Higher = matched the query better. Computed during filtering.
    score: i32,
}

pub fn render(
    ctx: &egui::Context,
    state: &mut PaletteState,
    chats: &[Chat],
    active_chat: Option<uuid::Uuid>,
) -> PaletteAction {
    if !state.open {
        return PaletteAction::None;
    }

    // Build the full action set.
    let mut entries = builtin_entries(active_chat);
    for c in chats {
        entries.push(Entry {
            title: format!("Go to: {}", c.title),
            subtitle: format!("{} · {}", c.provider, c.model),
            action: PaletteAction::SelectChat(c.id),
            score: 0,
        });
    }

    // Score against query (simple subsequence-ish fuzzy match).
    let q = state.query.to_lowercase();
    for e in &mut entries {
        e.score = if q.is_empty() {
            0
        } else {
            score(&e.title.to_lowercase(), &e.subtitle.to_lowercase(), &q)
        };
    }
    if !q.is_empty() {
        entries.retain(|e| e.score > i32::MIN);
        entries.sort_by(|a, b| b.score.cmp(&a.score));
    }
    if entries.is_empty() {
        state.cursor = 0;
    } else {
        state.cursor = state.cursor.min(entries.len() - 1);
    }

    // Keyboard nav. Reading before we lay anything out so the input handler
    // doesn't fight with the TextEdit.
    let mut chosen = false;
    ctx.input_mut(|i| {
        if i.consume_key(egui::Modifiers::NONE, Key::Escape) {
            state.open = false;
        }
        if !entries.is_empty() {
            if i.consume_key(egui::Modifiers::NONE, Key::ArrowDown) {
                state.cursor = (state.cursor + 1).min(entries.len() - 1);
            }
            if i.consume_key(egui::Modifiers::NONE, Key::ArrowUp) {
                state.cursor = state.cursor.saturating_sub(1);
            }
            if i.consume_key(egui::Modifiers::NONE, Key::Enter) {
                chosen = true;
            }
        }
    });

    let mut picked = PaletteAction::None;

    // Translucent backdrop intercepts clicks so the user can't accidentally
    // interact with the chat behind.
    let screen = ctx.content_rect();
    egui::Area::new(egui::Id::new("palette-backdrop"))
        .order(Order::Foreground)
        .fixed_pos(screen.min)
        .show(ctx, |ui| {
            let painter = ui.painter();
            painter.rect_filled(screen, 0.0, Color32::from_black_alpha(180));
            let resp = ui.allocate_rect(screen, egui::Sense::click());
            if resp.clicked() {
                state.open = false;
            }
        });

    egui::Area::new(egui::Id::new("palette"))
        .order(Order::Tooltip)
        .anchor(Align2::CENTER_TOP, egui::vec2(0.0, 120.0))
        .show(ctx, |ui| {
            ui.set_min_width(560.0);
            ui.set_max_width(640.0);
            Frame::new()
                .fill(crate::theme::ASSISTANT_BUBBLE)
                .stroke(Stroke::new(1.0, crate::theme::ACCENT.gamma_multiply(0.6)))
                .corner_radius(CornerRadius::same(14))
                .inner_margin(Margin::same(10))
                .shadow(egui::epaint::Shadow {
                    offset: [0, 8],
                    blur: 24,
                    spread: 0,
                    color: Color32::from_black_alpha(180),
                })
                .show(ui, |ui| {
                    let input = ui.add_sized(
                        [ui.available_width(), 32.0],
                        TextEdit::singleline(&mut state.query)
                            .hint_text(format!(
                                "{} Type a command or chat title…",
                                egui_phosphor::regular::MAGNIFYING_GLASS
                            ))
                            .frame(egui::Frame::NONE),
                    );
                    input.request_focus();

                    ui.add_space(6.0);
                    ui.separator();

                    egui::ScrollArea::vertical()
                        .max_height(360.0)
                        .show(ui, |ui| {
                            for (i, e) in entries.iter().enumerate() {
                                let selected = i == state.cursor;
                                let row = ui.allocate_response(
                                    egui::vec2(ui.available_width(), 36.0),
                                    egui::Sense::click(),
                                );
                                let bg = if selected {
                                    crate::theme::ACCENT.gamma_multiply(0.20)
                                } else if row.hovered() {
                                    ui.visuals().widgets.hovered.bg_fill
                                } else {
                                    Color32::TRANSPARENT
                                };
                                ui.painter()
                                    .rect_filled(row.rect, CornerRadius::same(8), bg);
                                let p = row.rect.shrink2(egui::vec2(10.0, 4.0));
                                ui.painter().text(
                                    p.left_top(),
                                    Align2::LEFT_TOP,
                                    &e.title,
                                    egui::TextStyle::Body.resolve(ui.style()),
                                    Color32::WHITE,
                                );
                                ui.painter().text(
                                    p.left_bottom() - egui::vec2(0.0, 2.0),
                                    Align2::LEFT_BOTTOM,
                                    &e.subtitle,
                                    egui::TextStyle::Small.resolve(ui.style()),
                                    Color32::from_rgb(150, 158, 168),
                                );
                                if row.hovered() {
                                    state.cursor = i;
                                }
                                if row.clicked() {
                                    picked = e.action.clone();
                                }
                            }
                        });

                    if chosen {
                        if let Some(e) = entries.get(state.cursor) {
                            picked = e.action.clone();
                        }
                    }

                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new("↵ run    ↑↓ select    esc close")
                                .small()
                                .color(Color32::from_rgb(140, 150, 160)),
                        );
                    });
                });
        });

    if !matches!(picked, PaletteAction::None) {
        state.open = false;
    }
    picked
}

fn builtin_entries(active_chat: Option<uuid::Uuid>) -> Vec<Entry> {
    let mut v = vec![
        Entry {
            title: "New chat".into(),
            subtitle: "⌘N".into(),
            action: PaletteAction::NewChat,
            score: 0,
        },
        Entry {
            title: "Open settings".into(),
            subtitle: "⌘,".into(),
            action: PaletteAction::OpenSettings,
            score: 0,
        },
        Entry {
            title: "Toggle voice mode".into(),
            subtitle: "⌘⇧V".into(),
            action: PaletteAction::ToggleVoice,
            score: 0,
        },
        Entry {
            title: "Toggle text-to-speech".into(),
            subtitle: String::new(),
            action: PaletteAction::ToggleTts,
            score: 0,
        },
        Entry {
            title: "Toggle RAG (semantic retrieval)".into(),
            subtitle: String::new(),
            action: PaletteAction::ToggleRag,
            score: 0,
        },
        Entry {
            title: "Theme: Cosmic".into(),
            subtitle: String::new(),
            action: PaletteAction::Theme(ThemeMode::Cosmic),
            score: 0,
        },
        Entry {
            title: "Theme: Dark".into(),
            subtitle: String::new(),
            action: PaletteAction::Theme(ThemeMode::Dark),
            score: 0,
        },
        Entry {
            title: "Theme: Light".into(),
            subtitle: String::new(),
            action: PaletteAction::Theme(ThemeMode::Light),
            score: 0,
        },
        Entry {
            title: "Provider: xAI Grok".into(),
            subtitle: String::new(),
            action: PaletteAction::Provider(Provider::Xai),
            score: 0,
        },
        Entry {
            title: "Provider: OpenAI".into(),
            subtitle: String::new(),
            action: PaletteAction::Provider(Provider::OpenAi),
            score: 0,
        },
        Entry {
            title: "Provider: Anthropic".into(),
            subtitle: String::new(),
            action: PaletteAction::Provider(Provider::Anthropic),
            score: 0,
        },
        Entry {
            title: "Provider: Local (Ollama)".into(),
            subtitle: String::new(),
            action: PaletteAction::Provider(Provider::Local),
            score: 0,
        },
        Entry {
            title: "Quit".into(),
            subtitle: String::new(),
            action: PaletteAction::Quit,
            score: 0,
        },
    ];
    if active_chat.is_some() {
        v.insert(
            2,
            Entry {
                title: "Export active chat (Markdown)".into(),
                subtitle: String::new(),
                action: PaletteAction::ExportActiveChat,
                score: 0,
            },
        );
    }
    v
}

/// Subsequence fuzzy score. Returns `i32::MIN` if `q` doesn't match.
fn score(title: &str, subtitle: &str, q: &str) -> i32 {
    let in_title = subseq_match(title, q);
    let in_sub = subseq_match(subtitle, q);
    let best = in_title.max(in_sub);
    if best < 0 {
        i32::MIN
    } else {
        // Hits in the title rank above hits in the subtitle.
        let title_bonus = if in_title >= 0 { 100 } else { 0 };
        title_bonus + best
    }
}

fn subseq_match(hay: &str, needle: &str) -> i32 {
    if needle.is_empty() {
        return 0;
    }
    // Walk the needle char-by-char, advancing the haystack iterator past each
    // match. `consumed` is the count of haystack chars we've seen so far; the
    // match position (0-based char index into `hay`) is therefore `consumed - 1`
    // at the moment of the match — no separate `enumerate()` needed, no
    // double-counting against `consumed`.
    let mut it = hay.chars();
    let mut last_pos: Option<usize> = None;
    let mut score: i32 = 0;
    let mut consumed = 0usize;
    for nc in needle.chars() {
        let mut matched = None;
        for hc in it.by_ref() {
            consumed += 1;
            if hc == nc {
                matched = Some(consumed - 1);
                break;
            }
        }
        match matched {
            Some(pos) => {
                // Adjacent matches score higher; gaps cost a small amount.
                if let Some(prev) = last_pos {
                    if pos == prev + 1 {
                        score += 5;
                    } else {
                        score -= (pos - prev) as i32;
                    }
                } else if pos == 0 {
                    score += 10; // prefix match
                }
                last_pos = Some(pos);
            }
            None => return -1,
        }
    }
    score
}
