#![allow(dead_code)]
//! Custom egui widget: animated bar-style waveform driven by a single
//! [0.0, 1.0] level. Callers feed the latest RMS level each frame; phase is
//! maintained externally so multiple instances on screen don't lock-step.

use crate::theme;
use egui::{Color32, CornerRadius, Pos2, Rect, Response, Sense, Stroke, StrokeKind, Ui, Vec2};

pub struct Waveform<'a> {
    level: f32,
    bars: usize,
    color: Color32,
    glow: bool,
    desired_size: Vec2,
    phase: &'a mut f32,
}

impl<'a> Waveform<'a> {
    pub fn new(level: f32, phase: &'a mut f32) -> Self {
        Self {
            level: level.clamp(0.0, 1.0),
            bars: 5,
            color: theme::ACCENT,
            glow: true,
            desired_size: Vec2::new(72.0, 28.0),
            phase,
        }
    }

    pub fn bars(mut self, n: usize) -> Self {
        self.bars = n.max(1);
        self
    }

    pub fn color(mut self, c: Color32) -> Self {
        self.color = c;
        self
    }

    pub fn size(mut self, size: Vec2) -> Self {
        self.desired_size = size;
        self
    }

    pub fn show(self, ui: &mut Ui) -> Response {
        let (rect, response) = ui.allocate_exact_size(self.desired_size, Sense::hover());
        let painter = ui.painter_at(rect);
        let dt = ui.input(|i| i.unstable_dt).clamp(0.0, 0.1);
        *self.phase += dt * 6.0;

        let n = self.bars;
        let bar_gap = 3.0;
        let bar_w = ((rect.width() - bar_gap * (n as f32 - 1.0)) / n as f32).max(2.0);
        let center_y = rect.center().y;
        let max_h = rect.height();

        for i in 0..n {
            let phase = *self.phase + i as f32 * 0.6;
            let envelope = (phase.sin() * 0.5 + 0.5) * 0.6 + 0.4;
            let height = (self.level * envelope * max_h).max(2.0);
            let x = rect.left() + i as f32 * (bar_w + bar_gap);
            let bar = Rect::from_min_size(
                Pos2::new(x, center_y - height / 2.0),
                Vec2::new(bar_w, height),
            );
            let radius = CornerRadius::same((bar_w / 2.0).round() as u8);
            if self.glow {
                painter.rect_filled(bar.expand(1.0), radius, self.color.gamma_multiply(0.25));
            }
            painter.rect_filled(bar, radius, self.color);
            painter.rect_stroke(
                bar,
                radius,
                Stroke::new(0.5, self.color.gamma_multiply(0.6)),
                StrokeKind::Outside,
            );
        }

        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(16));

        response
    }
}
