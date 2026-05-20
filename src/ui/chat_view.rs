//! Center panel: chat transcript + input.
//!
//! Assistant messages are rendered through `egui_commonmark` so Markdown,
//! tables, lists, and fenced code blocks all look right. User messages stay
//! plain text since they're the user's own input.

use crate::models::{Chat, Message, Role};
use crate::theme;
use egui::{Align, Color32, Layout, RichText, ScrollArea, Stroke, TextEdit, Ui, Vec2};
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};

#[derive(Debug, Default)]
pub struct ChatAction {
    pub send: Option<String>,
    pub stop: bool,
    pub copy: Option<String>,
    pub regenerate: Option<uuid::Uuid>,
    pub toggle_voice: bool,
    pub toggle_tts: bool,
}

pub struct ChatViewState {
    pub input: String,
    /// True while we have an in-flight assistant generation.
    pub streaming: bool,
    /// True when the realtime voice WS is active.
    pub voice_active: bool,
    /// True when TTS playback is currently audible.
    pub tts_speaking: bool,
    pub mic_level: f32,
    pub tts_enabled: bool,
    /// Cached phase used by the waveform widget.
    pub waveform_phase: f32,
    /// Shared markdown cache. Reused across renders so syntax-highlighted
    /// code blocks aren't re-parsed every frame.
    pub md_cache: CommonMarkCache,
    /// Render assistant messages as Markdown (default) vs plain text.
    pub render_markdown: bool,
}

impl Default for ChatViewState {
    fn default() -> Self {
        Self {
            input: String::new(),
            streaming: false,
            voice_active: false,
            tts_speaking: false,
            mic_level: 0.0,
            tts_enabled: true,
            waveform_phase: 0.0,
            md_cache: CommonMarkCache::default(),
            render_markdown: true,
        }
    }
}

pub fn render(
    ui: &mut Ui,
    state: &mut ChatViewState,
    chat: Option<&Chat>,
    messages: &[Message],
) -> ChatAction {
    let mut action = ChatAction::default();

    ui.vertical(|ui| {
        render_header(ui, chat);
        ui.separator();
        render_transcript(ui, state, messages, &mut action);
        render_composer(ui, state, &mut action);
    });

    action
}

fn render_header(ui: &mut Ui, chat: Option<&Chat>) {
    ui.horizontal(|ui| {
        ui.add_space(8.0);
        let title = chat
            .map(|c| c.title.clone())
            .unwrap_or_else(|| "GrokInsane".into());
        ui.heading(RichText::new(title).strong());
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if let Some(c) = chat {
                ui.label(
                    RichText::new(format!("{} · {}", c.provider, c.model))
                        .small()
                        .color(Color32::from_rgb(140, 150, 160)),
                );
                if c.pinned {
                    ui.label(RichText::new("📌").color(theme::ACCENT));
                }
            }
        });
    });
}

fn render_transcript(
    ui: &mut Ui,
    state: &mut ChatViewState,
    messages: &[Message],
    action: &mut ChatAction,
) {
    ScrollArea::vertical()
        .stick_to_bottom(true)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.add_space(8.0);
            if messages.is_empty() {
                ui.vertical_centered(|ui| {
                    ui.add_space(80.0);
                    ui.label(
                        RichText::new("ask me anything")
                            .size(20.0)
                            .color(Color32::from_rgb(170, 175, 180)),
                    );
                });
                return;
            }
            for m in messages {
                render_message(ui, state, m, action);
                ui.add_space(8.0);
            }
            ui.add_space(8.0);
        });
}

fn render_message(ui: &mut Ui, state: &mut ChatViewState, m: &Message, action: &mut ChatAction) {
    let is_user = matches!(m.role, Role::User);
    let bubble_fill = if is_user {
        theme::USER_BUBBLE
    } else {
        theme::ASSISTANT_BUBBLE
    };
    let max_width = ui.available_width().min(720.0);
    ui.with_layout(
        if is_user {
            Layout::right_to_left(Align::Min)
        } else {
            Layout::left_to_right(Align::Min)
        },
        |ui| {
            ui.set_max_width(ui.available_width());
            ui.allocate_ui_with_layout(
                Vec2::new(max_width, 0.0),
                Layout::top_down(if is_user { Align::Max } else { Align::Min }),
                |ui| {
                    egui::Frame::group(ui.style())
                        .fill(bubble_fill)
                        .stroke(Stroke::new(1.0, theme::BORDER))
                        .corner_radius(egui::CornerRadius::same(12))
                        .inner_margin(egui::Margin::symmetric(14, 10))
                        .show(ui, |ui| {
                            ui.set_max_width(max_width - 32.0);
                            ui.label(
                                RichText::new(m.role.as_str().to_uppercase())
                                    .small()
                                    .color(Color32::from_rgb(140, 150, 160)),
                            );
                            ui.add_space(2.0);
                            if is_user || !state.render_markdown {
                                ui.label(RichText::new(&m.content).color(Color32::WHITE));
                            } else {
                                CommonMarkViewer::new().max_image_width(Some(560)).show(
                                    ui,
                                    &mut state.md_cache,
                                    &m.content,
                                );
                            }
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(m.created_at.format("%H:%M:%S").to_string())
                                        .small()
                                        .color(Color32::from_rgb(110, 120, 130)),
                                );
                                if let Some(t) = m.tokens {
                                    ui.label(
                                        RichText::new(format!("· {t} tok"))
                                            .small()
                                            .color(Color32::from_rgb(110, 120, 130)),
                                    );
                                }
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    if ui.small_button("copy").clicked() {
                                        action.copy = Some(m.content.clone());
                                    }
                                    if !is_user
                                        && ui
                                            .small_button("↻")
                                            .on_hover_text("regenerate")
                                            .clicked()
                                    {
                                        action.regenerate = Some(m.id);
                                    }
                                });
                            });
                        });
                },
            );
        },
    );
}

fn render_composer(ui: &mut Ui, state: &mut ChatViewState, action: &mut ChatAction) {
    ui.separator();
    ui.add_space(4.0);

    let composer_height = 96.0;
    egui::Frame::group(ui.style())
        .fill(theme::ASSISTANT_BUBBLE)
        .stroke(Stroke::new(1.0, theme::BORDER))
        .corner_radius(egui::CornerRadius::same(12))
        .inner_margin(egui::Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_min_height(composer_height);
            ui.horizontal_top(|ui| {
                let input_resp = ui.add(
                    TextEdit::multiline(&mut state.input)
                        .hint_text(if state.streaming {
                            "generating… press ⌘. to stop"
                        } else {
                            "message Grok (⏎ send, ⇧⏎ newline)"
                        })
                        .desired_rows(3)
                        .desired_width(f32::INFINITY)
                        .frame(egui::Frame::NONE),
                );
                let key_send = ui.input(|i| {
                    i.key_pressed(egui::Key::Enter) && !i.modifiers.shift && input_resp.has_focus()
                });
                if key_send {
                    submit_input(state, action);
                }

                ui.vertical(|ui| {
                    let mic_label = if state.voice_active {
                        egui_phosphor::regular::STOP_CIRCLE
                    } else {
                        egui_phosphor::regular::MICROPHONE
                    };
                    if ui
                        .add_sized(
                            [38.0, 32.0],
                            egui::Button::new(RichText::new(mic_label).color(Color32::WHITE))
                                .fill(if state.voice_active {
                                    Color32::from_rgb(180, 70, 90)
                                } else {
                                    Color32::from_rgb(40, 50, 60)
                                })
                                .corner_radius(8),
                        )
                        .on_hover_text("Toggle voice mode (⌘⇧V)")
                        .clicked()
                    {
                        action.toggle_voice = true;
                    }

                    let tts_label = if state.tts_enabled {
                        egui_phosphor::regular::SPEAKER_HIGH
                    } else {
                        egui_phosphor::regular::SPEAKER_SLASH
                    };
                    if ui
                        .add_sized(
                            [38.0, 32.0],
                            egui::Button::new(RichText::new(tts_label).color(Color32::WHITE))
                                .fill(if state.tts_speaking {
                                    theme::ACCENT.gamma_multiply(0.35)
                                } else {
                                    Color32::from_rgb(40, 50, 60)
                                })
                                .corner_radius(8),
                        )
                        .on_hover_text("Toggle Grok speaking aloud")
                        .clicked()
                    {
                        action.toggle_tts = true;
                    }

                    if state.streaming {
                        if ui
                            .add_sized(
                                [38.0, 32.0],
                                egui::Button::new(
                                    RichText::new(egui_phosphor::regular::STOP)
                                        .color(Color32::WHITE),
                                )
                                .fill(Color32::from_rgb(120, 60, 60))
                                .corner_radius(8),
                            )
                            .on_hover_text("Stop generation (⌘.)")
                            .clicked()
                        {
                            action.stop = true;
                        }
                    } else if ui
                        .add_sized(
                            [38.0, 32.0],
                            egui::Button::new(
                                RichText::new(egui_phosphor::regular::PAPER_PLANE_RIGHT)
                                    .color(Color32::WHITE),
                            )
                            .fill(theme::ACCENT.gamma_multiply(0.4))
                            .corner_radius(8),
                        )
                        .on_hover_text("Send (⏎)")
                        .clicked()
                    {
                        submit_input(state, action);
                    }
                });
            });

            // Show waveform when recording.
            if state.voice_active {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    crate::ui::waveform::Waveform::new(state.mic_level, &mut state.waveform_phase)
                        .size(Vec2::new(120.0, 24.0))
                        .show(ui);
                    ui.label(
                        RichText::new("listening…")
                            .small()
                            .color(Color32::from_rgb(160, 170, 180)),
                    );
                });
            }
        });
}

fn submit_input(state: &mut ChatViewState, action: &mut ChatAction) {
    let trimmed = state.input.trim();
    if trimmed.is_empty() || state.streaming {
        return;
    }
    action.send = Some(trimmed.to_string());
    state.input.clear();
}
