//! Cloudscape-inspired visual tokens and reusable widgets.
use eframe::egui::{self, Color32, CornerRadius, FontId, Margin, RichText, Stroke, StrokeKind, Ui};

#[derive(Clone, Copy)]
pub struct Palette {
    pub bg: Color32,
    pub container: Color32,
    pub container_alt: Color32,
    pub border: Color32,
    pub text: Color32,
    pub text_dim: Color32,
    pub accent: Color32,
    pub success: Color32,
    pub warning: Color32,
    pub danger: Color32,
    pub topnav: Color32,
    pub nav: Color32,
    pub series: [Color32; 5],
}

const fn rgb(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
}

pub fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            bg: rgb(0x0f, 0x14, 0x1a),
            container: rgb(0x16, 0x1d, 0x26),
            container_alt: rgb(0x1b, 0x23, 0x2e),
            border: rgb(0x35, 0x41, 0x50),
            text: rgb(0xe9, 0xeb, 0xed),
            text_dim: rgb(0x9b, 0xa7, 0xb6),
            accent: rgb(0x42, 0x9a, 0xe8),
            success: rgb(0x2e, 0xa5, 0x97),
            warning: rgb(0xe0, 0xa4, 0x1a),
            danger: rgb(0xff, 0x7a, 0x7a),
            topnav: rgb(0x0b, 0x0e, 0x12),
            nav: rgb(0x12, 0x18, 0x20),
            series: [rgb(0x42, 0x9a, 0xe8), rgb(0xe0, 0x79, 0x41), rgb(0x2e, 0xa5, 0x97), rgb(0xb3, 0x8a, 0xe6), rgb(0xe0, 0xa4, 0x1a)],
        }
    } else {
        Palette {
            bg: rgb(0xf3, 0xf3, 0xf7),
            container: rgb(0xff, 0xff, 0xff),
            container_alt: rgb(0xf9, 0xf9, 0xfb),
            border: rgb(0xc6, 0xc6, 0xcd),
            text: rgb(0x0f, 0x14, 0x1a),
            text_dim: rgb(0x5f, 0x6b, 0x7a),
            accent: rgb(0x09, 0x72, 0xd3),
            success: rgb(0x03, 0x7f, 0x0c),
            warning: rgb(0x8d, 0x6c, 0x00),
            danger: rgb(0xd9, 0x1e, 0x18),
            topnav: rgb(0x0f, 0x14, 0x1a),
            nav: rgb(0xfa, 0xfa, 0xfc),
            series: [rgb(0x09, 0x72, 0xd3), rgb(0xc5, 0x53, 0x05), rgb(0x00, 0x80, 0x7a), rgb(0x68, 0x4a, 0xc7), rgb(0x8d, 0x6c, 0x00)],
        }
    }
}

pub fn pal(ui: &Ui) -> Palette {
    palette(ui.visuals().dark_mode)
}

pub fn apply(ctx: &egui::Context, dark: bool) {
    let p = palette(dark);
    let mut v = if dark { egui::Visuals::dark() } else { egui::Visuals::light() };
    v.panel_fill = p.bg;
    v.window_fill = p.container;
    v.extreme_bg_color = p.container_alt;
    v.faint_bg_color = p.container_alt;
    v.window_stroke = Stroke::new(1.0, p.border);
    v.override_text_color = Some(p.text);
    v.hyperlink_color = p.accent;
    v.selection.bg_fill = p.accent.gamma_multiply(0.35);
    v.selection.stroke = Stroke::new(1.0, p.accent);
    let r = CornerRadius::same(4);
    for w in [&mut v.widgets.noninteractive, &mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
        w.corner_radius = r;
    }
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    v.widgets.inactive.bg_fill = p.container;
    v.widgets.inactive.weak_bg_fill = p.container;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, p.border);
    v.widgets.hovered.bg_fill = p.container_alt;
    v.widgets.hovered.weak_bg_fill = p.container_alt;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, p.accent);
    v.widgets.active.bg_stroke = Stroke::new(1.5, p.accent);
    ctx.set_visuals_of(if dark { egui::Theme::Dark } else { egui::Theme::Light }, v);
    ctx.global_style_mut(|s| {
        s.spacing.item_spacing = egui::vec2(8.0, 8.0);
        s.spacing.button_padding = egui::vec2(12.0, 5.0);
        s.spacing.interact_size.y = 28.0;
        s.text_styles.insert(egui::TextStyle::Heading, FontId::proportional(22.0));
        s.text_styles.insert(egui::TextStyle::Body, FontId::proportional(14.0));
        s.text_styles.insert(egui::TextStyle::Button, FontId::proportional(14.0));
        s.text_styles.insert(egui::TextStyle::Small, FontId::proportional(12.0));
        s.text_styles.insert(egui::TextStyle::Monospace, FontId::monospace(13.0));
    });
}

pub use crate::linux::Kind;

pub fn dot(ui: &mut Ui, color: Color32) {
    let (r, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
    ui.painter().circle_filled(r.center(), 4.0, color);
}

pub fn kind_color(p: &Palette, k: Kind) -> Color32 {
    match k {
        Kind::Success => p.success,
        Kind::Warning => p.warning,
        Kind::Error => p.danger,
        Kind::Info => p.accent,
        Kind::Inactive => p.text_dim,
    }
}

/// Cloudscape status indicator: coloured dot + label.
pub fn status(ui: &mut Ui, k: Kind, text: &str) {
    let p = pal(ui);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let (r, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
        ui.painter().circle_filled(r.center(), 4.5, kind_color(&p, k));
        ui.label(text);
    });
}

/// Container with a header, the Cloudscape "card".
pub fn container<R>(ui: &mut Ui, title: &str, add: impl FnOnce(&mut Ui) -> R) -> R {
    container_with(ui, title, |_| {}, add)
}

pub fn container_with<R>(ui: &mut Ui, title: &str, header_extra: impl FnOnce(&mut Ui), add: impl FnOnce(&mut Ui) -> R) -> R {
    let p = pal(ui);
    let fill = FILL_H.with(|c| c.replace(0.0));
    egui::Frame::new()
        .fill(p.container)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(8)
        .inner_margin(Margin::same(16))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            if fill > 0.0 {
                ui.set_min_height((fill - 34.0).max(0.0));
            }
            ui.horizontal(|ui| {
                ui.label(RichText::new(title).size(16.0).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), header_extra);
            });
            ui.add_space(4.0);
            add(ui)
        })
        .inner
}

pub fn kv(ui: &mut Ui, key: &str, value: impl Into<String>) {
    let p = pal(ui);
    ui.horizontal(|ui| {
        ui.label(RichText::new(key).color(p.text_dim));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add(egui::Label::new(RichText::new(value.into())).truncate());
        });
    });
}

/// Large value with caption, used in dashboard cards.
pub fn big_value(ui: &mut Ui, caption: &str, value: &str, color: Option<Color32>) {
    let p = pal(ui);
    ui.label(RichText::new(caption).color(p.text_dim).size(12.0));
    ui.label(RichText::new(value).size(28.0).strong().color(color.unwrap_or(p.text)));
}

pub fn usage_color(p: &Palette, pct: f32) -> Color32 {
    if pct >= 90.0 { p.danger } else if pct >= 75.0 { p.warning } else { p.accent }
}

/// Horizontal meter, `frac` in 0..=1.
pub fn meter(ui: &mut Ui, frac: f32, color: Color32, height: f32) {
    let p = pal(ui);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), height), egui::Sense::hover());
    let r = CornerRadius::same((height / 2.0) as u8);
    ui.painter().rect_filled(rect, r, p.border.gamma_multiply(0.5));
    let mut f = rect;
    f.set_width((rect.width() * frac.clamp(0.0, 1.0)).max(if frac > 0.0 { height } else { 0.0 }));
    ui.painter().rect_filled(f, r, color);
}

pub fn primary_button(ui: &mut Ui, text: &str) -> egui::Response {
    let p = pal(ui);
    ui.add(egui::Button::new(RichText::new(text).strong().color(Color32::WHITE)).fill(p.accent))
}

pub fn danger_button(ui: &mut Ui, text: &str) -> egui::Response {
    let p = pal(ui);
    ui.add(egui::Button::new(RichText::new(text).strong().color(Color32::WHITE)).fill(p.danger.gamma_multiply(0.85)))
}

pub fn badge(ui: &mut Ui, text: &str, color: Color32) {
    let galley = ui.painter().layout_no_wrap(text.to_string(), FontId::proportional(12.0), Color32::WHITE);
    let (rect, _) = ui.allocate_exact_size(galley.size() + egui::vec2(12.0, 4.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(10), color);
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, Color32::WHITE);
}

pub fn note(ui: &mut Ui, k: Kind, text: &str) {
    let p = pal(ui);
    let c = kind_color(&p, k);
    egui::Frame::new()
        .fill(c.gamma_multiply(0.12))
        .stroke(Stroke::new(1.0, c.gamma_multiply(0.7)))
        .corner_radius(6)
        .inner_margin(Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(text);
        });
}

pub fn page_title(ui: &mut Ui, title: &str, subtitle: &str) {
    let p = pal(ui);
    ui.add_space(4.0);
    ui.label(RichText::new(title).heading().strong());
    if !subtitle.is_empty() {
        ui.label(RichText::new(subtitle).color(p.text_dim));
    }
    ui.add_space(6.0);
}

thread_local! {
    /// Height the next `container` should stretch to (set by `card_grid` so a row has equal heights).
    static FILL_H: std::cell::Cell<f32> = const { std::cell::Cell::new(0.0) };
}

/// Responsive card grid: balanced rows, cards fill the width, equal height within a row.
pub fn card_grid(ui: &mut Ui, min_col: f32, n: usize, mut f: impl FnMut(&mut Ui, usize)) {
    let gap = 12.0;
    let w = ui.available_width();
    let max_cols = (((w + gap) / (min_col + gap)).floor() as usize).clamp(1, n.max(1));
    let rows = n.div_ceil(max_cols).max(1);
    let cols = n.div_ceil(rows).max(1);
    let mut i = 0;
    for r in 0..rows {
        let count = (n - i).min(cols);
        if count == 0 {
            break;
        }
        let col_w = (w - gap * (count as f32 - 1.0)) / count as f32;
        let key = ui.id().with(("cardrow", r, cols, (w / 8.0) as i32, n));
        let prev: f32 = ui.ctx().data(|d| d.get_temp(key)).unwrap_or(0.0);
        let mut maxh = 0.0f32;
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            for _ in 0..count {
                let resp = ui.allocate_ui_with_layout(egui::vec2(col_w, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                    ui.set_width(col_w);
                    FILL_H.with(|c| c.set(prev));
                    f(ui, i);
                    FILL_H.with(|c| c.set(0.0));
                });
                maxh = maxh.max(resp.response.rect.height());
                i += 1;
            }
        });
        if (maxh - prev).abs() > 0.5 {
            ui.ctx().data_mut(|d| d.insert_temp(key, maxh));
            ui.ctx().request_repaint();
        }
        ui.add_space(gap - 8.0);
    }
}

pub fn outline(ui: &Ui, rect: egui::Rect, color: Color32) {
    ui.painter().rect_stroke(rect, CornerRadius::same(4), Stroke::new(1.0, color), StrokeKind::Inside);
}
