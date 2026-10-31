//! Memory metrics from /proc/meminfo.
#[cfg(feature = "gui")]
use crate::charts::{line_chart, Series};
use crate::linux::read_trim;
#[cfg(feature = "gui")]
use crate::linux::human_bytes;
#[cfg(feature = "gui")]
use crate::monitor::Snapshot;
#[cfg(feature = "gui")]
use crate::theme::{self, pal};
#[cfg(feature = "gui")]
use eframe::egui::{RichText, Ui};
use std::collections::{HashMap, VecDeque};

#[derive(Default, Clone)]
pub struct Mem {
    pub total: u64,
    pub used: u64,
    pub available: u64,
    pub free: u64,
    pub cached: u64,
    pub buffers: u64,
    pub swap_total: u64,
    pub swap_used: u64,
    pub hist: VecDeque<f32>,
    pub hist_swap: VecDeque<f32>,
}

impl Mem {
    pub fn pct(&self) -> f32 {
        if self.total == 0 { 0.0 } else { 100.0 * self.used as f32 / self.total as f32 }
    }
    pub fn swap_pct(&self) -> f32 {
        if self.swap_total == 0 { 0.0 } else { 100.0 * self.swap_used as f32 / self.swap_total as f32 }
    }
}

pub fn sample(m: &mut Mem, cap: usize) {
    let txt = read_trim("/proc/meminfo").unwrap_or_default();
    let kv: HashMap<&str, u64> = txt
        .lines()
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            Some((k, v.trim().trim_end_matches("kB").trim().parse::<u64>().ok()? * 1024))
        })
        .collect();
    let g = |k: &str| kv.get(k).copied().unwrap_or(0);
    m.total = g("MemTotal");
    m.free = g("MemFree");
    m.available = kv.get("MemAvailable").copied().unwrap_or(m.free + g("Cached") + g("Buffers"));
    m.used = m.total.saturating_sub(m.available);
    m.buffers = g("Buffers");
    m.cached = g("Cached") + g("SReclaimable");
    m.swap_total = g("SwapTotal");
    m.swap_used = m.swap_total.saturating_sub(g("SwapFree"));
    let (pc, sp) = (m.pct(), m.swap_pct());
    crate::linux::push(&mut m.hist, pc, cap);
    crate::linux::push(&mut m.hist_swap, sp, cap);
}

#[cfg(feature = "gui")]
pub fn page(ui: &mut Ui, s: &Snapshot) {
    let m = &s.mem;
    let p = pal(ui);
    theme::page_title(ui, "Memory", &format!("{} installed", human_bytes(m.total)));
    theme::card_grid(ui, 200.0, 4, |ui, i| {
        theme::container(ui, ["Used", "Available", "Cached", "Swap"][i], |ui| match i {
            0 => {
                theme::big_value(ui, &human_bytes(m.used), &format!("{:.1}%", m.pct()), Some(theme::usage_color(&p, m.pct())));
                theme::meter(ui, m.pct() / 100.0, theme::usage_color(&p, m.pct()), 6.0);
            }
            1 => theme::big_value(ui, "Reclaimable included", &human_bytes(m.available), None),
            2 => {
                theme::big_value(ui, "Page cache + slab", &human_bytes(m.cached), None);
                ui.label(RichText::new(format!("buffers {}", human_bytes(m.buffers))).color(p.text_dim));
            }
            _ => {
                if m.swap_total == 0 {
                    theme::big_value(ui, "Swap", "none", None);
                } else {
                    theme::big_value(ui, &format!("{} of {}", human_bytes(m.swap_used), human_bytes(m.swap_total)), &format!("{:.1}%", m.swap_pct()), Some(theme::usage_color(&p, m.swap_pct())));
                    theme::meter(ui, m.swap_pct() / 100.0, theme::usage_color(&p, m.swap_pct()), 6.0);
                }
            }
        });
    });
    theme::container(ui, "Utilization history", |ui| {
        line_chart(
            ui,
            220.0,
            s.cap,
            &[Series { name: "RAM %", color: p.series[0], data: &m.hist }, Series { name: "Swap %", color: p.series[1], data: &m.hist_swap }],
            Some(100.0),
            &|v| format!("{v:.0}%"),
        );
    });
    ui.add_space(8.0);
    theme::container(ui, "Breakdown", |ui| {
        for (label, v) in [("Total", m.total), ("Used", m.used), ("Free", m.free), ("Available", m.available), ("Cached", m.cached), ("Buffers", m.buffers), ("Swap total", m.swap_total), ("Swap used", m.swap_used)] {
            theme::kv(ui, label, human_bytes(v));
        }
    });
}
