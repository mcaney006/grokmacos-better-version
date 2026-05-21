//! Settings panel — modal window with API key, model, voice, etc.

use crate::models::{PerfStats, Provider, Settings, ThemeMode, VoicePersona};
use crate::ui::perf_dashboard;
use crate::ui::toast::Toaster;
use egui::{Context, RichText, ScrollArea, TextEdit};
use zeroize::Zeroize as _;

#[derive(Default)]
pub struct SettingsState {
    pub open: bool,
    /// API key text-edit buffer. We don't wrap this in `Zeroizing`
    /// because egui's `TextBuffer` trait isn't implemented for any
    /// wrapper type, and egui's `TextEdit` reallocates the buffer
    /// internally as the user types — those intermediate allocations
    /// would leak unzeroed regardless of wrapper. The pragmatic
    /// protection is `clear_securely` below: called when the dialog
    /// closes, wipes the live bytes before the buffer goes idle.
    pub api_key_buffer: String,
    pub api_key_dirty: bool,
}

impl SettingsState {
    /// Wipe the api-key bytes from RAM. Called on dialog close + after
    /// a successful save. NOT a full guarantee against memory scraping
    /// (intermediate egui reallocations may have leaked older copies
    /// while typing); a best-effort scrub of the live buffer.
    pub fn clear_securely(&mut self) {
        self.api_key_buffer.zeroize();
        self.api_key_dirty = false;
    }
}

#[derive(Debug, Default, Clone)]
pub struct SettingsAction {
    pub close: bool,
    pub save_settings: bool,
    pub save_api_key: bool,
    pub clear_api_key: bool,
    pub rebuild_index: bool,
}

pub fn render(
    ctx: &Context,
    state: &mut SettingsState,
    settings: &mut Settings,
    stats: &PerfStats,
    _toaster: &mut Toaster,
) -> SettingsAction {
    let mut action = SettingsAction::default();
    if !state.open {
        return action;
    }
    let mut open_flag = state.open;
    egui::Window::new(RichText::new("Settings").strong())
        .open(&mut open_flag)
        .resizable(true)
        .collapsible(false)
        .default_size([520.0, 520.0])
        .show(ctx, |ui| {
            ScrollArea::vertical().show(ui, |ui| {
                ui.heading("API");
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label("Provider:");
                    egui::ComboBox::from_id_salt("provider_select")
                        .selected_text(settings.default_provider.label())
                        .show_ui(ui, |ui| {
                            for p in Provider::all() {
                                if ui
                                    .selectable_value(&mut settings.default_provider, *p, p.label())
                                    .changed()
                                {
                                    action.save_settings = true;
                                }
                            }
                        });
                });

                ui.horizontal(|ui| {
                    let label = match settings.default_provider {
                        Provider::Xai => "xAI API key",
                        Provider::OpenAi => "OpenAI API key",
                        Provider::Anthropic => "Anthropic API key",
                        Provider::Local => "Local endpoint (URL)",
                    };
                    ui.label(label);
                    let resp = ui.add_sized(
                        [320.0, 24.0],
                        TextEdit::singleline(&mut state.api_key_buffer)
                            .password(matches!(
                                settings.default_provider,
                                Provider::Xai | Provider::OpenAi | Provider::Anthropic
                            ))
                            .hint_text("paste key, then click save"),
                    );
                    if resp.changed() {
                        state.api_key_dirty = true;
                    }
                });
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(state.api_key_dirty, egui::Button::new("Save key"))
                        .clicked()
                    {
                        action.save_api_key = true;
                        state.api_key_dirty = false;
                    }
                    if ui.button("Clear stored key").clicked() {
                        action.clear_api_key = true;
                        state.api_key_buffer.clear();
                        state.api_key_dirty = false;
                    }
                });

                ui.add_space(8.0);
                ui.separator();

                ui.heading("Model");
                ui.add_space(4.0);
                let (model_label, model_field) = match settings.default_provider {
                    Provider::Xai => ("xAI model", &mut settings.xai_model),
                    Provider::OpenAi => ("OpenAI model", &mut settings.openai_model),
                    Provider::Anthropic => ("Anthropic model", &mut settings.anthropic_model),
                    Provider::Local => ("Local model id", &mut settings.local_model),
                };
                ui.horizontal(|ui| {
                    ui.label(model_label);
                    if ui
                        .add_sized([240.0, 24.0], TextEdit::singleline(model_field))
                        .changed()
                    {
                        action.save_settings = true;
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Temperature");
                    if ui
                        .add(egui::Slider::new(&mut settings.temperature, 0.0..=2.0).step_by(0.05))
                        .changed()
                    {
                        action.save_settings = true;
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Max tokens");
                    if ui
                        .add(
                            egui::DragValue::new(&mut settings.max_tokens)
                                .range(64..=131_072)
                                .speed(64),
                        )
                        .changed()
                    {
                        action.save_settings = true;
                    }
                });

                ui.add_space(8.0);
                ui.separator();
                ui.heading("Appearance");
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label("Theme");
                    if ui
                        .selectable_value(&mut settings.theme, ThemeMode::Cosmic, "Cosmic")
                        .changed()
                    {
                        action.save_settings = true;
                    }
                    if ui
                        .selectable_value(&mut settings.theme, ThemeMode::Dark, "Dark")
                        .changed()
                    {
                        action.save_settings = true;
                    }
                    if ui
                        .selectable_value(&mut settings.theme, ThemeMode::Light, "Light")
                        .changed()
                    {
                        action.save_settings = true;
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Font size");
                    if ui
                        .add(egui::Slider::new(&mut settings.font_size, 10.0..=24.0).step_by(0.5))
                        .changed()
                    {
                        action.save_settings = true;
                    }
                });

                ui.add_space(8.0);
                ui.separator();
                ui.heading("Voice");
                ui.add_space(4.0);
                if ui
                    .checkbox(&mut settings.tts_enabled, "Speak Grok's replies")
                    .changed()
                {
                    action.save_settings = true;
                }
                ui.horizontal(|ui| {
                    ui.label("Personality");
                    egui::ComboBox::from_id_salt("persona_select")
                        .selected_text(format!("{:?}", settings.voice_persona))
                        .show_ui(ui, |ui| {
                            for p in VoicePersona::all() {
                                if ui
                                    .selectable_value(
                                        &mut settings.voice_persona,
                                        *p,
                                        format!("{:?} — {}", p, p.description()),
                                    )
                                    .changed()
                                {
                                    action.save_settings = true;
                                }
                            }
                        });
                });

                ui.add_space(8.0);
                ui.separator();
                ui.heading("Retrieval");
                ui.add_space(4.0);
                if ui
                    .checkbox(
                        &mut settings.rag_enabled,
                        "Augment prompts with relevant past messages",
                    )
                    .changed()
                {
                    action.save_settings = true;
                }
                ui.horizontal(|ui| {
                    ui.label("Top K");
                    if ui
                        .add(egui::Slider::new(&mut settings.rag_top_k, 1..=12))
                        .changed()
                    {
                        action.save_settings = true;
                    }
                });
                if ui.button("Rebuild search index").clicked() {
                    action.rebuild_index = true;
                }

                ui.add_space(8.0);
                ui.separator();
                ui.heading("System prompt");
                ui.add_space(4.0);
                let mut sp = settings.system_prompt.clone().unwrap_or_default();
                if ui
                    .add(
                        TextEdit::multiline(&mut sp)
                            .desired_width(f32::INFINITY)
                            .desired_rows(4)
                            .hint_text("optional — prepended to every conversation"),
                    )
                    .changed()
                {
                    settings.system_prompt = if sp.trim().is_empty() { None } else { Some(sp) };
                    action.save_settings = true;
                }

                ui.add_space(8.0);
                ui.separator();
                if ui
                    .checkbox(&mut settings.perf_dashboard, "Show performance dashboard")
                    .changed()
                {
                    action.save_settings = true;
                }
                if settings.perf_dashboard {
                    ui.add_space(8.0);
                    perf_dashboard::render(ui, stats);
                }
            });
        });
    state.open = open_flag;
    if !state.open {
        action.close = true;
    }
    action
}
