//! Lightweight performance dashboard rendered inside the settings panel.

use crate::models::PerfStats;
use egui::Ui;

pub fn render(ui: &mut Ui, stats: &PerfStats) {
    ui.group(|ui| {
        ui.heading("Performance");
        ui.add_space(4.0);
        egui::Grid::new("perf_grid")
            .num_columns(2)
            .striped(true)
            .show(ui, |ui| {
                ui.label("Frame time");
                ui.monospace(format!("{:.2} ms", stats.frame_ms));
                ui.end_row();
                ui.label("Frame rate");
                ui.monospace(format!("{:.0} fps", stats.fps));
                ui.end_row();
                ui.label("Tokens / s");
                ui.monospace(format!("{:.1}", stats.tokens_per_sec));
                ui.end_row();
                ui.label("Last request");
                ui.monospace(format!("{} ms", stats.last_request_ms));
                ui.end_row();
                ui.label("Indexed messages");
                ui.monospace(format!("{}", stats.messages_indexed));
                ui.end_row();
                ui.label("Resident memory");
                ui.monospace(humansize(stats.mem_bytes));
                ui.end_row();
            });
    });
}

fn humansize(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut i = 0;
    while value >= 1024.0 && i + 1 < UNITS.len() {
        value /= 1024.0;
        i += 1;
    }
    format!("{:.1} {}", value, UNITS[i])
}
