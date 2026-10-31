//! Lightweight real-time line/area chart drawn with the egui painter.
use crate::theme::pal;
use eframe::egui::{self, epaint, Align2, Color32, FontId, Pos2, Rect, Sense, Shape, Stroke, Ui};
use std::collections::VecDeque;

pub struct Series<'a> {
    pub name: &'a str,
    pub color: Color32,
    pub data: &'a VecDeque<f32>,
}

fn nice_max(v: f32) -> f32 {
    if v <= 0.0 {
        return 1.0;
    }
    let mag = 10f32.powf(v.log10().floor());
    for m in [1.0, 2.0, 2.5, 5.0, 10.0] {
        if v <= m * mag {
            return m * mag;
        }
    }
    10.0 * mag
}

/// `cap` is the number of samples spanning the full width (newest is rightmost).
pub fn line_chart(ui: &mut Ui, height: f32, cap: usize, series: &[Series], y_max: Option<f32>, fmt: &dyn Fn(f32) -> String) {
    let p = pal(ui);
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, height), Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 4, p.container_alt);

    let data_max = series.iter().flat_map(|s| s.data.iter().copied()).fold(0.0f32, f32::max);
    let ymax = y_max.unwrap_or_else(|| nice_max(data_max * 1.1));
    let left = 72.0;
    let plot = Rect::from_min_max(Pos2::new(rect.left() + left, rect.top() + 8.0), Pos2::new(rect.right() - 10.0, rect.bottom() - 8.0));
    let grid = Stroke::new(1.0, p.border.gamma_multiply(0.55));
    for i in 0..=4 {
        let f = i as f32 / 4.0;
        let y = plot.bottom() - f * plot.height();
        painter.line_segment([Pos2::new(plot.left(), y), Pos2::new(plot.right(), y)], grid);
        painter.text(Pos2::new(plot.left() - 6.0, y), Align2::RIGHT_CENTER, fmt(ymax * f), FontId::proportional(11.0), p.text_dim);
    }
    let n = cap.max(2);
    let xs = |i: usize, len: usize| -> f32 {
        // right-align: sample k of len sits at slot (n - len + k)
        let slot = n - len.min(n) + i;
        plot.left() + plot.width() * slot as f32 / (n - 1) as f32
    };
    let ys = |v: f32| plot.bottom() - (v / ymax).clamp(0.0, 1.0) * plot.height();

    for s in series {
        let len = s.data.len().min(n);
        if len < 2 {
            continue;
        }
        let skip = s.data.len() - len;
        let pts: Vec<Pos2> = s.data.iter().skip(skip).enumerate().map(|(i, v)| Pos2::new(xs(i, len), ys(*v))).collect();
        let mut mesh = epaint::Mesh::default();
        let top = s.color.gamma_multiply(0.28);
        let bot = s.color.gamma_multiply(0.02);
        for pt in &pts {
            mesh.colored_vertex(*pt, top);
            mesh.colored_vertex(Pos2::new(pt.x, plot.bottom()), bot);
        }
        for i in 0..pts.len() as u32 - 1 {
            let b = i * 2;
            mesh.add_triangle(b, b + 1, b + 2);
            mesh.add_triangle(b + 1, b + 3, b + 2);
        }
        painter.add(Shape::mesh(mesh));
        painter.add(Shape::line(pts, Stroke::new(1.8, s.color)));
    }

    if let Some(pos) = resp.hover_pos() {
        if plot.contains(pos) {
            let longest = series.iter().map(|s| s.data.len()).max().unwrap_or(0).min(n);
            if longest > 0 {
                let slot = (((pos.x - plot.left()) / plot.width()) * (n - 1) as f32).round() as isize;
                let idx = slot - (n - longest) as isize;
                if idx >= 0 && (idx as usize) < longest {
                    let x = xs(idx as usize, longest);
                    painter.line_segment([Pos2::new(x, plot.top()), Pos2::new(x, plot.bottom())], Stroke::new(1.0, p.text_dim));
                    let secs = longest as isize - 1 - idx;
                    egui::Area::new(ui.id().with("chart_tip")).order(egui::Order::Tooltip).fixed_pos(pos + egui::vec2(14.0, 10.0)).show(ui.ctx(), |ui| {
                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                            ui.label(egui::RichText::new(if secs == 0 { "now".to_string() } else { format!("{secs}s ago") }).small());
                            for s in series {
                                let off = s.data.len() as isize - longest as isize;
                                if let Some(v) = s.data.get((idx + off).max(0) as usize) {
                                    ui.horizontal(|ui| {
                                        crate::theme::dot(ui, s.color);
                                        ui.label(format!("{}: {}", s.name, fmt(*v)));
                                    });
                                }
                            }
                        });
                    });
                }
            }
        }
    }
    if series.len() > 1 || series.first().map(|s| !s.name.is_empty()).unwrap_or(false) {
        ui.horizontal(|ui| {
            for s in series {
                crate::theme::dot(ui, s.color);
                ui.label(egui::RichText::new(s.name).small());
                ui.add_space(8.0);
            }
        });
    }
}

