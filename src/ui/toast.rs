//! Minimal toast queue rendered at the bottom-right of the window.

use egui::{Align2, Color32, Vec2};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastLevel {
    Info,
    Warn,
    Error,
}

pub struct Toast {
    pub level: ToastLevel,
    pub text: String,
    pub created_at: Instant,
    pub duration: Duration,
}

#[derive(Default)]
pub struct Toaster {
    items: Vec<Toast>,
}

impl Toaster {
    pub fn push(&mut self, level: ToastLevel, text: impl Into<String>) {
        self.items.push(Toast {
            level,
            text: text.into(),
            created_at: Instant::now(),
            duration: Duration::from_secs(5),
        });
    }

    pub fn info(&mut self, text: impl Into<String>) {
        self.push(ToastLevel::Info, text);
    }
    pub fn warn(&mut self, text: impl Into<String>) {
        self.push(ToastLevel::Warn, text);
    }
    pub fn error(&mut self, text: impl Into<String>) {
        self.push(ToastLevel::Error, text);
    }

    pub fn render(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        self.items
            .retain(|t| now.duration_since(t.created_at) < t.duration);
        if self.items.is_empty() {
            return;
        }
        egui::Area::new(egui::Id::new("grok-insane-toasts"))
            .anchor(Align2::RIGHT_BOTTOM, Vec2::new(-16.0, -16.0))
            .interactable(false)
            .show(ctx, |ui| {
                ui.set_max_width(360.0);
                for toast in &self.items {
                    let color = match toast.level {
                        ToastLevel::Info => Color32::from_rgb(60, 110, 180),
                        ToastLevel::Warn => Color32::from_rgb(180, 140, 50),
                        ToastLevel::Error => Color32::from_rgb(190, 70, 70),
                    };
                    egui::Frame::group(ui.style())
                        .fill(color.gamma_multiply(0.15))
                        .stroke(egui::Stroke::new(1.0, color))
                        .corner_radius(egui::CornerRadius::same(8))
                        .inner_margin(egui::Margin::symmetric(12, 8))
                        .show(ui, |ui| {
                            ui.label(egui::RichText::new(&toast.text).color(Color32::WHITE));
                        });
                    ui.add_space(6.0);
                }
            });
        // Keep the UI refreshing so toasts disappear when their TTL expires.
        ctx.request_repaint_after(Duration::from_millis(250));
    }
}
